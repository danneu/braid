# Fix: lock.rs Pass 2 silently demotes `DuplicateDevid` corruption to orphan

## Context

`braid lock` builds its mapper-close set in `cli/src/lock.rs::build_close_sets_full`. Pass 2 iterates `pool.null_underlying` and uses `membership.by_devid(nu.devid)` to decide between `MemberOwned` (a known member) and `Orphan` (an unowned mapper, safe to close). The current implementation collapses every non-`Ok(Some)` outcome into a catch-all that routes the mapper to `orphan_mappers` and closes it:

```
match membership.by_devid(nu.devid) {        // cli/src/lock.rs:819
    Ok(Some((_uuid, member))) => member_owned.push(...),
    _ => { orphan_mappers.push(...) }
}
```

`PoolMembership::by_devid` returns `Result<Option<(&LuksUuid, &DiskMember)>, MembershipError>` (`cli/src/membership.rs:284`); in practice it only constructs the `MembershipError::DuplicateDevid` variant. The `_ =>` arm therefore lumps together "no member matches this devid" (legitimate orphan, close it) with "two members claim this devid, membership is corrupt" (cannot prove ownership). This is structurally fragile and the only `by_devid` call site in the codebase that discards the structured corruption variant:

- `cli/src/remove_missing.rs:62` -- `?`-propagates `MembershipError::DuplicateDevid` into `RemoveMissingError::Membership`, aborting the destructive op.
- `cli/src/recover.rs:1593-1611` -- explicit match, converts to `JournaledSnapshotError::DuplicateDevid`, aborts replay.

**Reachability:** this branch is unreachable in `braid lock`'s current flow. `load_membership_from` (`cli/src/membership.rs:459-473`) rejects duplicate-devid corruption at load time as `MembershipError::Conflict`, and `lock.rs` never calls `enrich_from_pool_state`, so the in-memory `PoolMembership` Pass 2 sees cannot have duplicate devids. The fix is **defensive consistency**, not a live correctness fix. Value:

1. Removes the brittle catch-all so a future change to lock's flow (e.g., adding live enrichment before classification) cannot accidentally trigger a destructive close on a corrupt-membership mapper.
2. Aligns the third `by_devid` call site with how the other two treat `DuplicateDevid`.
3. Decision 024 explicitly enumerates `MembershipError` variants so operator remediation is enumerable (`docs/decisions/024-luks-uuid-identity.md`); collapsing them at the call site defeats that design intent.

## Recommended approach (per user selection)

Match `Err(MembershipError::DuplicateDevid { devid, members })` explicitly in Pass 2 and route it through lock.rs's existing best-effort cleanup idiom -- the same shape `push_uuid_classified_candidate` (Pass 3) uses for unverifiable candidates:

- Emit a `PreviewNote::Warn` whose body names the colliding UUIDs.
- Push the mapper into `skipped_mappers` (NOT `orphan_mappers`).
- Set `cleanup_uncertain = true`.
- Leave the mapper open (skip the close).

Lock proceeds to close every verified member; the operator sees a `[warn]` line plus the `[info] cleanup incomplete` note that `LockPlan::preview` already appends when `cleanup_uncertain` is set (`cli/src/lock.rs:457`). Graceful-shutdown UX is preserved.

## Files to modify

### 1. `cli/src/lock.rs`

**Imports (line 4).** Extend the existing `use crate::membership::PoolMembership;` to also import `MembershipError`. Add `LuksUuid` to the existing `use crate::types::...` line (already imports `DiskName`, `MapperName`, etc.; `LuksUuid` is referenced by the new helper signature). Add `use crate::types::format_uuid_list;` (currently `pub(crate)` at `cli/src/types.rs:67`).

**New warn-body helpers (insert near the existing helpers, around `cli/src/lock.rs:226-256`).** Models the existing pattern of one helper per warn shape so dry-run preview and real-run stderr share wording. Two helpers -- one for the structured `DuplicateDevid` case (specific, operator-actionable wording), one for any other `MembershipError` variant that future refactors may surface from `by_devid` (generic, leans on the variant's `Display` impl):

```rust
/// Message body (no `[warn]` prefix) for a `null_underlying` mapper
/// whose persisted devid collides across two or more membership UUIDs.
/// `pool.json` is internally inconsistent; lock cannot prove ownership,
/// so the mapper is left open and the colliding UUIDs are surfaced so
/// the operator can repair pool.json (e.g., `braid discover --write`)
/// before the next lock.
fn duplicate_devid_warn_body(
    entry: &MapperName,
    devid: u64,
    members: &[LuksUuid],
) -> String {
    format!(
        "skipping mapper {entry}: pool.json has duplicate devid {devid} across UUIDs {} -- leaving open; repair pool.json before next lock",
        format_uuid_list(members),
    )
}

/// Generic fallback warn body for any non-`DuplicateDevid`
/// `MembershipError` that `by_devid` might surface in the future
/// (today it only returns `Ok` or `DuplicateDevid`, but the type
/// allows the full enum). Leans on the variant's `Display` impl so
/// each future variant brings its own operator-facing wording.
fn membership_error_warn_body(entry: &MapperName, err: &MembershipError) -> String {
    format!(
        "skipping mapper {entry}: {err} -- leaving open; repair pool.json before next lock"
    )
}
```

`duplicate_devid_warn_body` reuses `format_uuid_list` (`cli/src/types.rs:67`) -- the same helper that `MembershipError::DuplicateDevid`'s Display impl uses (`cli/src/membership.rs:47`), so the warn body and the underlying error message stay aligned.

**Pass 2 rewrite (`cli/src/lock.rs:817-836`).** Replace the catch-all with four explicit arms. The match is on `Result<Option<...>, MembershipError>`, and `MembershipError` has five variants (`Corrupt`, `Conflict`, `DuplicateDevid`, `Io`, `Save` -- `cli/src/membership.rs:29-66`), so the compiler will NOT consider `Ok(Some)` + `Ok(None)` + `Err(DuplicateDevid)` exhaustive. A fail-closed `Err(err)` arm covers the remaining variants today and any future ones:

```rust
// Pass 2: pool.null_underlying, classified by persisted devid.
for nu in &pool.null_underlying {
    match membership.by_devid(nu.devid) {
        Ok(Some((_uuid, member))) => member_owned.push(LockMapperClose {
            mapper: nu.mapper.clone(),
            kind: LockMapperCloseKind::MemberOwned {
                display_name: member.name.clone(),
            },
        }),
        Ok(None) => {
            // Legitimate orphan: persisted devid is not in membership.
            let disk_name = name_from_mapper(nu.mapper.as_str())
                .unwrap_or(nu.mapper.as_str())
                .to_owned();
            orphan_mappers.push(LockMapperClose {
                mapper: nu.mapper.clone(),
                kind: LockMapperCloseKind::Orphan { disk_name },
            });
        }
        Err(MembershipError::DuplicateDevid { devid, members }) => {
            // Corrupt membership: two UUIDs claim this devid. Lock
            // cannot prove ownership -- warn + skip + mark uncertain
            // (Pass 3's pattern for unverifiable candidates).
            notes.push(PreviewNote::Warn(duplicate_devid_warn_body(
                &nu.mapper, devid, &members,
            )));
            skipped_mappers.push(nu.mapper.clone());
            *cleanup_uncertain = true;
        }
        Err(err) => {
            // Fail-closed for any other MembershipError variant. Today
            // `by_devid` only returns `Ok` or `DuplicateDevid`, but the
            // signature allows the full enum; this arm makes the match
            // exhaustive at the type level and absorbs any future variant
            // by leaning on its `Display` impl for the warn body.
            notes.push(PreviewNote::Warn(membership_error_warn_body(
                &nu.mapper, &err,
            )));
            skipped_mappers.push(nu.mapper.clone());
            *cleanup_uncertain = true;
        }
    }
}
```

**Pass 3 already-observed fix (`cli/src/lock.rs:843-849`).** The current `already_observed` set is built only from `member_owned` and `orphan_mappers`; it works today because Pass 1 and Pass 2 each route every input mapper to one of those vectors. Once Pass 2 also pushes to `skipped_mappers`, a `pool.null_underlying` mapper that exists in `/dev/mapper` would be re-scanned as a Pass 3 "stranded" candidate -- producing a duplicate warn (different wording from `skipped_mapper_warn_body`) and another `skipped_mappers` entry, and exposing the operator to a backing-device race reclassification window. The fix is to derive `already_observed` from the INPUT (`pool.devices` plus `pool.null_underlying`) rather than the intermediate output, restoring the comment's stated invariant ("did NOT appear in pool.devices or pool.null_underlying"):

```rust
let stranded = {
    let already_observed: HashSet<&str> = pool
        .devices
        .iter()
        .map(|d| d.mapper.as_str())
        .chain(pool.null_underlying.iter().map(|nu| nu.mapper.as_str()))
        .collect();

    match scan_braid_mapper_candidates(fs, &already_observed) {
        Ok(entries) => entries,
        Err(e) => {
            notes.push(PreviewNote::Warn(mapper_scan_warn_body(&e)));
            return LockCloseSet::from_classified(member_owned, orphan_mappers);
        }
    }
};
```

This is byte-equivalent to today's behavior on legitimate paths (`member_owned ∪ orphan_mappers == pool.devices ∪ pool.null_underlying` when no skip happens) and correctly excludes the new skipped mapper.

### 2. `cli/src/membership.rs`

**Test-only corruption constructor on `PoolMembership` (insert near the existing `by_devid_returns_duplicate_devid_on_corruption` test, around line 936, OR as a top-level `#[cfg(test)] impl`).** Mirrors the same-module backdoor that test uses (`cli/src/membership.rs:955-957`), but exposes it as a labeled, downstream-test-callable helper:

```rust
#[cfg(test)]
impl PoolMembership {
    /// Test-only constructor that bypasses `PoolMembership::insert`'s
    /// four-axis uniqueness check so downstream tests (e.g., `lock.rs`)
    /// can build a corrupt membership for defensive-coding coverage.
    /// Production paths cannot reach this constructor.
    pub(crate) fn for_corruption_tests(
        entries: Vec<(LuksUuid, DiskMember)>,
    ) -> Self {
        let mut disks: BTreeMap<LuksUuid, DiskMember> = BTreeMap::new();
        for (uuid, member) in entries {
            disks.insert(uuid, member);
        }
        PoolMembership { disks: LuksUuidMap(disks) }
    }
}
```

This is the only sanctioned way to bypass the four-axis check; existing production code remains fail-closed.

### 3. New unit test in `cli/src/lock.rs`

Add a test next to the other `build_close_sets_full` unit tests. Required preamble per `docs/testing.md`:

- **Intent.** When `build_close_sets_full` encounters a `null_underlying` mapper whose persisted devid collides across two membership UUIDs, the mapper is pushed to `skipped_mappers` (not `orphan_mappers`), `cleanup_uncertain` is set, exactly one `PreviewNote::Warn` is emitted naming both colliding UUIDs, and Pass 3 does NOT re-scan the same mapper from `/dev/mapper`.
- **Why it exists.** The previous `_ =>` catch-all silently routed `MembershipError::DuplicateDevid` to `orphan_mappers` and closed the device. This pins the defensive branch so a regression that reintroduces the catch-all -- or that reverts Pass 3's `already_observed` derivation to the intermediate-output form -- is caught by `just test-rust`.
- **Scenario.** A corrupt `pool.json` was loaded in memory (via the `for_corruption_tests` backdoor) where two members carry `devid: Some(7)`. The pool has a `null_underlying` mapper `braid-X` at devid 7, and `/dev/mapper/braid-X` exists (seeded via `MockFs`). Lock cannot prove ownership; the mapper must end up skipped exactly once with no duplicate Pass 3 warning.

Assertions:

- `member_summaries(&close_set)` (`cli/src/lock.rs:2484`) does NOT contain a `braid-X` mapper entry.
- `orphan_summaries(&close_set)` (`cli/src/lock.rs:2498`) does NOT contain a `braid-X` mapper entry.
- `skipped_mappers` contains `braid-X` exactly once (pin the count -- a regression in Pass 3's `already_observed` would absorb `braid-X` through `push_uuid_classified_candidate` and produce a second `skipped_mappers` entry).
- `cleanup_uncertain == true`.
- Exactly one `PreviewNote::Warn` was pushed (pin the count for the same Pass-3 reason); its body contains the substring `"duplicate devid 7"` and both UUIDs in canonical-lex order (compare against the order `format_uuid_list` produces).
- `runner.requests()` (`cli/src/cmd.rs:1104`) contains zero `CmdRequest::CryptsetupStatus { mapper, .. }` entries whose `mapper.as_str() == "braid-X"`. This is the load-bearing Pass-3 invariance assertion: it pins that `scan_braid_mapper_candidates` excluded `braid-X` from `stranded` (rather than relying on the fact that a missing mock would happen to surface as a second skip).

Test setup:

- Build the corrupt membership via `PoolMembership::for_corruption_tests(vec![(u1, m1), (u2, m2)])` with both `m1.devid = Some(7)` and `m2.devid = Some(7)`.
- Build the `PoolState` with one `null_underlying` entry `{ mapper: MapperName("braid-X"), devid: 7 }` and `pool.devices: vec![]` so Pass 1 is empty and the test focuses on Pass 2 + Pass 3.
- Build the `MockFs` so `/dev/mapper` lists `braid-X` (i.e., it would be a stranded candidate if Pass 3 saw it). The Pass-3 `scan_braid_mapper_candidates` calls `fs.list_dir("/dev/mapper")` and `fs.exists("/dev/mapper/braid-X")`; both must be configured to return the mapper.
- Build the `MockRunner` with NO `CryptsetupStatus` / `CryptsetupLuksUuid` outputs configured for `braid-X`. Note: a missing mock surfaces as `CmdError::MissingMock` (`cli/src/cmd.rs:1080`), which `classify_candidate_mapper` propagates to `push_uuid_classified_candidate`'s `Err(cmd_err)` arm (`cli/src/lock.rs:278-285`), producing a `skipped_mapper_warn_body` warn and another `skipped_mappers` entry -- which the count assertions above would catch indirectly. The direct check is the `runner.requests()` assertion above, which proves Pass 3 never issued the probe at all.

Do NOT reuse `lock_test_membership` (`cli/src/test_fixtures/lock.rs:127`) -- it produces a clean membership through the public `insert` API and cannot carry duplicate devids.

## Verification

1. `just test-rust` -- the new lock-Pass-2 corruption test and the existing membership / parser / lock tests all pass.
2. `cargo build -p braid-cli` -- compiles cleanly; the four-arm match (`Ok(Some)` / `Ok(None)` / `Err(DuplicateDevid)` / `Err(_)` bound) is exhaustive over the `MembershipError` enum at the type level, with no `_ =>` catch-all on the `Result` itself.
3. `rg "by_devid" cli/src/` -- all three call sites (`lock.rs`, `remove_missing.rs`, `recover.rs`) now handle `MembershipError::DuplicateDevid` explicitly; lock.rs additionally fails closed on any other variant.
4. Manually trace the test scenario through Pass 3: with `pool.null_underlying` mapper names included in `already_observed`, `scan_braid_mapper_candidates` returns an empty `stranded` vec, and the test's "no extra warn / no second skipped entry" assertions hold.
5. No NixOS VM test is required: the corrupt-membership state is unreachable via real `braid lock` (gated by `load_membership_from`'s uniqueness sweep). The unit test pinning the defensive branch + Pass 3 filtering is the contract.

## Out of scope

- Changing `load_membership_from` to produce `MembershipError::DuplicateDevid` instead of `MembershipError::Conflict` for duplicate-devid corruption at load. The load-time wording is already consistent with the other Conflict variants and is not the call site under scrutiny.
- Touching the inner `match e` redundancy at `cli/src/recover.rs:1605-1608` (dead code -- `DuplicateDevid` is already matched on the preceding arm). Unrelated cleanup.
- Adding similar defensive branches elsewhere -- `remove_missing.rs:62` and `recover.rs:1593-1611` already handle `DuplicateDevid` explicitly.
- Adjusting decision 024 wording. The decision document does not directly reference this branch; no doc update is required.

## Files touched (summary)

- `cli/src/lock.rs` -- two new warn-body helpers (`duplicate_devid_warn_body`, `membership_error_warn_body`); rewrite of Pass 2 into a four-arm exhaustive match; rewrite of Pass 3's `already_observed` derivation to use `pool.devices` + `pool.null_underlying` mapper names; one new unit test; imports of `MembershipError`, `LuksUuid`, and `format_uuid_list`.
- `cli/src/membership.rs` -- new `#[cfg(test)] pub(crate) fn for_corruption_tests` constructor on `PoolMembership`.
