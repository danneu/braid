# Plan: doctor `declared_disks` detects offline (verified-but-unassembled) members

> Intended canonical path on pickup: `plans/wip/2026-06-01-doctor-offline-declared-disks.md`.
> (Written to the plan-mode scratch file; rename on pickup. `plans/wip/` is gitignored.)

## Context

Commit `ffcdc222 feat(status): render verified unpooled members as offline` taught
`braid status` and the TUI to render a declared member as **offline** when its by-id
path is present, its on-disk LUKS UUID matches the `pool.json` membership key, but its
UUID is absent from the live btrfs pool (e.g. a degraded mount that dropped one member,
or an interrupted post-commit mutation).

`braid doctor` still has the blind spot the status work flagged as out of scope:
`check_declared_disks` classifies that same member as `DiskState::LuksHeaderOk` and
`summarize_declared_disks` reports the pool as fully healthy (`declared_disks` = `ok`).
Doctor never cross-checks declared members against live-pool membership. The follow-up
recorded in `plans/impl/2026-06-01-offline-disk-state-label.md` (`## Follow Up`) is:

> `cli/src/doctor.rs`: teach `declared_disks` to cross-check declared members against
> live-pool membership so present, LUKS-identity-verified but unassembled members are
> not reported as healthy.

**Outcome:** when the pool is mounted, a present + identity-verified member that is not
assembled into the live btrfs pool makes `declared_disks` **Warn** (cause-neutral),
distinct from the `Fail` reserved for a LUKS UUID mismatch. When the pool is offline,
`declared_disks` behavior is unchanged.

## Decisions (and rationale)

- **Severity = Warn, not Fail.** The member's LUKS identity is *valid*
  (`LuksHeaderOk` already means observed UUID == expected UUID). The cause is
  ambiguous -- a locked member in a degraded mount, an interrupted post-commit
  mutation, or mid-reconciliation topology -- so per
  `docs/design/decisions/024-luks-uuid-identity.md` (`## Offline Disk State`) the
  state is deliberately cause-neutral. `Fail` stays reserved for the unsafe identity
  contradiction (UUID mismatch = a foreign/swapped disk), a different safety class.
  No existing doctor invariant forces Fail here (verified by reading every
  `summarize_declared_disks` branch and `check_foreign_luks_uuid`).
- **Fold into `declared_disks`, no new check.** The follow-up note says "teach
  `declared_disks`"; status renders this as the disk's own state; `declared_disks`
  already owns "is each declared member healthy." A separate check would split one
  member's verdict across two rows.
- **Three live-topology states, not two.** `ensure_pool_state` only probes when
  mounted, so `ctx.pool_state` is `None` when the pool is offline (or config absent),
  `Some(Ok)` when mounted + probed, and `Some(Err)` when mounted but `probe_pool`
  failed. Collapsing `Some(Err)` into the offline path (as a two-state design would)
  silently reports `ok` and overclaims health. Map them to:
  - **Offline** (pool not mounted): no btrfs membership to compare against, so
    `declared_disks` keeps its identity-only behavior. Unchanged from today. Unlike
    `check_foreign_luks_uuid` (which *skips* when unmounted), declared_disks must keep
    validating LUKS identity offline, so it does not skip.
  - **Online** (mounted + probe Ok): cross-check member UUIDs against the live device
    set; an absent verified member becomes `Offline` (Warn).
  - **Unavailable** (mounted + probe Err): topology indeterminate. Do **not** claim
    health and do **not** fabricate `Offline`. Keep per-member LUKS identity
    classification and add a check-level Warn `could not compare declared disks to live
    pool: {e}`, mirroring `check_foreign_luks_uuid`'s probe-error warn. A UUID mismatch
    still dominates to Fail.
- **Reclassify only `LuksHeaderOk`.** The cross-check upgrades *only* a verified
  member to `Offline`. A `Missing` / `NotBlock` / `LuksUuidMismatch` / `ProbeFailed`
  / `LuksHeaderUnreadable` member keeps its state, so a real problem is never masked
  by "offline".

## Current state (verified, line numbers may drift -- re-grep)

- `cli/src/doctor.rs:172` -- `pool_state: Option<Result<PoolState, ProbeError>>` on
  `DoctorContext`.
- `cli/src/doctor.rs:290` -- `enum DiskState { LuksHeaderOk, LuksUuidMismatch{..},
  Missing, NotBlock, ProbeFailed(String), LuksHeaderUnreadable }`.
- `cli/src/doctor.rs:323` -- `classify_disk_state` gates on **real**
  `std::fs::metadata(path)` (line 328), then delegates to `classify_luks_identity`.
  This fs gate is why the new logic cannot be unit-tested through `check_declared_disks`
  (no MockRunner fabricates a real block device); see the doc comment at lines 313-318.
- `cli/src/doctor.rs:340` -- `classify_luks_identity` returns `LuksHeaderOk` when
  observed UUID == expected (line 362).
- `cli/src/doctor.rs:382` -- `summarize_declared_disks` buckets states; `LuksHeaderOk`
  is "no problem"; Warn for missing/not-block/probe/header-unreadable; Fail only when
  `uuid_mismatch` is non-empty (lines 473-477).
- `cli/src/doctor.rs:508` -- `check_declared_disks` loads membership, maps each member
  through `classify_disk_state`, discards the member UUID, calls
  `summarize_declared_disks`. Never touches `PoolState`.
- `cli/src/doctor.rs:619` -- `ensure_pool_state` caches `probe_pool` into
  `ctx.pool_state`; only probes when config present **and** mountpoint mounted; no-ops
  (leaves `None`) when offline.
- `cli/src/doctor.rs:845` -- `check_foreign_luks_uuid`: the **inverse** cross-check
  (live device whose UUID is not in membership) and the template for the
  `ensure_pool_state` read idiom. On `Some(Err(e))` it returns
  `CheckResult::warn(NAME, "could not probe pool: {e}")` (~lines 860-870) -- the
  mounted-pool probe-error precedent `declared_disks` now matches. (A second idiom that
  collapses to `Option<&PoolState>` exists at lines 1156-1157, but declared_disks needs
  the three-way `None`/`Ok`/`Err` distinction, so it matches on `ctx.pool_state`
  directly.)
- `cli/src/types.rs` -- `PoolState.devices: Vec<PoolDevice>`; `PoolDevice.luks_uuid:
  LuksUuid`; `PoolState::underlying_for_uuid` (a UUID-presence-ish lookup, not reused
  here -- we need a set).
- `cli/src/status.rs:981` -- status builds `HashSet<&LuksUuid>` from
  `pool.devices`; classification at lines 1057-1089; no `Action:` hint for `Offline`
  (lines 1461-1483 fall-through). `build_status` returns `not_mounted_status` before
  `build_disk_reports` when `!pool.mounted` (the status-side offline gate to mirror).
- `cli/src/luks.rs:750` -- `classify_member_luks_identity`. **Not needed in doctor**:
  doctor's existing `LuksHeaderOk` already encodes "identity verified". The only new
  comparison is pool-membership presence.
- `cli/src/membership.rs:670` -- `foreign_luks_uuids` is the membership-layer inverse.
  We do **not** add a symmetric membership helper: the offline decision is per-member
  and must only fire for `LuksHeaderOk`, so it lives next to the classification, not in
  membership.

## Implementation

### 1. Add `DiskState::Offline` (`cli/src/doctor.rs`, enum at ~290)

Add a fieldless variant after `LuksHeaderOk`:

```rust
/// Present, LUKS-identity-verified, and recorded in membership, but the
/// member's UUID is absent from the live btrfs pool. Only reachable under
/// `LiveTopology::Online`; cause-neutral (Warn), distinct from the unsafe
/// `LuksUuidMismatch`. Mirrors `status::DiskStatus::Offline`.
Offline,
```

### 2. Add a `LiveTopology` type (`cli/src/doctor.rs`)

Make the three states first-class so "mounted but unprobeable" cannot collapse into
"offline". Owned `HashSet<LuksUuid>` (not status's borrowed set) so the immutable borrow
of `ctx.pool_state` ends before the per-member loop -- avoids borrow-checker friction
with the `&mut ctx` in `ensure_pool_state` / the `ctx.runner` reads below.

```rust
/// Live btrfs topology as `declared_disks` sees it. Distinguishes "pool
/// offline -- nothing to compare" from "pool mounted but probe failed --
/// cannot verify assembly", so the latter warns instead of silently
/// reporting healthy (mirrors `check_foreign_luks_uuid`'s probe-error warn).
enum LiveTopology {
    /// Pool not mounted (or config absent); identity-only behavior preserved.
    Offline,
    /// Pool mounted and probed; UUIDs of assembled members.
    Online(HashSet<LuksUuid>),
    /// Pool mounted but `probe_pool` failed; topology indeterminate.
    Unavailable(String),
}
```

(Add `use std::collections::HashSet;` if not already imported.)

### 3. Pure cross-check helper (`cli/src/doctor.rs`, near `classify_disk_state`)

Isolate the per-member topology decision so it is unit-testable without the
`std::fs::metadata` gate. Only `Online` reclassifies; `Offline` and `Unavailable` leave
the member's state untouched (the "could not compare" signal is check-level, not
per-disk -- see step 5).

```rust
/// Cross-check a declared member's LUKS-verified state against live btrfs
/// topology. Upgrades a verified member to `Offline` only when the pool is
/// online and the member's UUID is not assembled into it. Every other base
/// state, and every non-`Online` topology, returns the state unchanged -- so a
/// real problem is never masked and a probe failure never fabricates `Offline`.
fn reconcile_with_live_pool(
    uuid: &LuksUuid,
    state: DiskState,
    topology: &LiveTopology,
) -> DiskState {
    match (&state, topology) {
        (DiskState::LuksHeaderOk, LiveTopology::Online(live)) if !live.contains(uuid) => {
            DiskState::Offline
        }
        _ => state,
    }
}
```

### 4. Wire the three-state consult into `check_declared_disks` (~508)

Resolve `LiveTopology` once, then reconcile each member's base state and pass the
unavailable reason (if any) to the summarizer:

```rust
fn check_declared_disks<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    let pool_membership = match load_membership_or_check_result(ctx, "declared_disks") {
        Ok(m) => m,
        Err(cr) => return cr,
    };

    // `ensure_pool_state` only probes when mounted, so: offline => pool_state stays
    // None (identity-only behavior preserved); mounted+probed => Some(Ok); mounted+
    // probe-failure => Some(Err) -- which must warn, not silently report healthy.
    let topology = if ensure_mountpoint_is_mounted(ctx) == Some(true) {
        ensure_pool_state(ctx);
        match ctx
            .pool_state
            .as_ref()
            .expect("ensure_pool_state seeds the cache when config is present and mounted")
        {
            Ok(pool) if pool.mounted => {
                LiveTopology::Online(pool.devices.iter().map(|d| d.luks_uuid.clone()).collect())
            }
            // Probe succeeded but reports not-mounted (authoritative btrfs probe
            // disagreeing with the mountpoint check): nothing to compare.
            Ok(_) => LiveTopology::Offline,
            Err(e) => LiveTopology::Unavailable(e.to_string()),
        }
    } else {
        LiveTopology::Offline
    };

    let topology_unavailable = match &topology {
        LiveTopology::Unavailable(e) => Some(e.as_str()),
        _ => None,
    };

    let members = pool_membership.iter_by_name();
    let classifications: Vec<(String, String, DiskState)> = members
        .into_iter()
        .map(|(uuid, member)| {
            let by_id = member.by_id.as_str().to_owned();
            let base = classify_disk_state(ctx.runner, Path::new(&by_id), uuid);
            let state = reconcile_with_live_pool(uuid, base, &topology);
            (member.name.as_str().to_owned(), by_id, state)
        })
        .collect();

    summarize_declared_disks(&classifications, topology_unavailable)
}
```

Note: this now initiates the pool probe earlier than before, but `ensure_pool_state`
caches, so the later `check_pool_missing_devices` etc. reuse it -- still one probe, no
duplicate work.

### 5. Handle `Offline` and topology-unavailable in `summarize_declared_disks` (~382)

Two additions: the per-disk `Offline` bucket, and a new check-level
`topology_unavailable: Option<&str>` parameter for the mounted-but-unprobeable case.

- Signature becomes `summarize_declared_disks(classifications: &[(String, String,
  DiskState)], topology_unavailable: Option<&str>)`. Update existing test call sites to
  pass `None` (see Tests).
- Add `let mut offline: Vec<String> = Vec::new();` and the match arm
  `DiskState::Offline => offline.push(format!("{name} ({by_id})")),`.
- Compute `disk_problem_count` = missing + not_block + probe_failed + header_unreadable +
  uuid_mismatch + **offline** lengths.
- Early `ok` only when `disk_problem_count == 0 && topology_unavailable.is_none()`.
- Add the `Offline` parts entry (cause-neutral, **remedy-free** -- no `Action`, no
  "run X" -- preserving the ffcdc222 / decision-024 posture):

```rust
if !offline.is_empty() {
    parts.push(format!(
        "{} present but not in the live pool: {}",
        offline.len(),
        offline.join(", ")
    ));
}
```

- Build the message so the global probe-error note rides alongside per-disk problems but
  is **not** counted as a per-disk problem (counting it would inflate the
  "{n}/{total} disks" framing for a fault that belongs to no specific disk):

```rust
let message = if disk_problem_count > 0 {
    let mut m = format!(
        "{disk_problem_count}/{total} {} problems: {}",
        if total == 1 { "disk has" } else { "disks have" },
        parts.join("; ")
    );
    if let Some(reason) = topology_unavailable {
        m.push_str(&format!("; could not compare declared disks to live pool: {reason}"));
    }
    m
} else {
    // No per-disk problems; only the live-pool comparison failed.
    format!(
        "could not compare declared disks to live pool: {}",
        topology_unavailable.expect("non-ok path with zero disk problems implies unavailable")
    )
};
```

- Severity unchanged in spirit: `if uuid_mismatch.is_empty() { warn } else { fail }`.
  Neither `offline` nor `topology_unavailable` populates `uuid_mismatch`, so each yields
  Warn on its own, while a UUID mismatch still dominates to Fail.

## Tests

### Unit tests (`cli/src/doctor.rs` `#[cfg(test)]`, reuse the `cls(...)` helper)

First, the signature change in step 5 forces a mechanical update: every existing
`summarize_declared_disks(&inputs)` call site in the test module gains a `, None` second
argument (`summarize_ok_when_all_headers_intact`, `summarize_warn_luks_header_unreadable`,
`summarize_declared_disks_promotes_to_fail_on_uuid_mismatch`, ...). No behavior change.

Pure summarizer (mirrors `summarize_ok_when_all_headers_intact` etc. at ~3196):

1. `summarize_warn_offline_member_not_in_live_pool` -- `summarize_declared_disks(&[cls("disk1", "/dev/disk/by-id/wwn-0x1", DiskState::Offline)], None)`
   -> `status == CheckStatus::Warn`; message contains `disk1` and
   `not in the live pool`. Pin cause-neutrality: assert message does **not** contain
   `Action` and does **not** contain `luksHeaderRestore`.
2. `summarize_offline_does_not_override_uuid_mismatch_fail` -- `&[cls(.., LuksUuidMismatch{..}), cls(.., DiskState::Offline)]`, `None`
   -> `status == CheckStatus::Fail`; mismatch guidance still present. Pins that offline
   does not downgrade Fail.
3. `summarize_warn_topology_unavailable_when_probe_failed` -- `summarize_declared_disks(&[cls("disk1", .., DiskState::LuksHeaderOk)], Some("boom"))`
   -> `status == CheckStatus::Warn`; message contains `could not compare declared disks
   to live pool` and `boom`. **This is the regression guard the finding asks for**: it
   fails if a mounted-pool probe error is silently treated as healthy (`Ok`) /
   offline-pool behavior.
4. `summarize_topology_unavailable_does_not_override_uuid_mismatch_fail` --
   `summarize_declared_disks(&[cls("disk1", .., LuksUuidMismatch{..})], Some("boom"))`
   -> `status == CheckStatus::Fail`; message contains both the mismatch guidance and
   `could not compare`. Pins that the probe-error note never downgrades a mismatch Fail.

Pure cross-check (`reconcile_with_live_pool`, no fs/runner needed; build `LuksUuid` via
the existing `test_uuid(n)` / `LuksUuid::parse`, and a `LiveTopology` value directly):

5. `reconcile_marks_verified_member_offline_when_absent_from_live_pool` -- `LuksHeaderOk`
   + `LiveTopology::Online(set without the member's UUID)` -> `Offline`.
6. `reconcile_keeps_verified_member_ok_when_present_in_live_pool` -- `LuksHeaderOk` +
   `LiveTopology::Online(set containing the UUID)` -> `LuksHeaderOk`.
7. `reconcile_keeps_state_when_pool_offline` -- `LuksHeaderOk` + `LiveTopology::Offline`
   -> `LuksHeaderOk`. (Directly pins pool-offline behavior at the helper level.)
8. `reconcile_unavailable_topology_does_not_fabricate_offline` -- `LuksHeaderOk` +
   `LiveTopology::Unavailable("boom".into())` -> `LuksHeaderOk` (never `Offline`). Pins
   that a probe error does not invent an offline verdict at the per-member layer.
9. `reconcile_never_masks_real_problem` -- `Missing` (and `LuksUuidMismatch{..}`) +
   `LiveTopology::Online(empty set)` -> returned unchanged. Pins that only `LuksHeaderOk`
   is upgraded.

These nine are behavioral and structure-insensitive (assert on `CheckStatus`, message
substrings, and `DiskState` variant). They cover the summarizer rendering and the
per-member reconcile logic. Two coverage gaps remain by construction: the
`Online -> Offline` reclassification needs a real `LuksHeaderOk` member (blocked by the
`std::fs::metadata` gate in `classify_disk_state`), covered by the VM test; and the
`check_declared_disks` wiring that feeds `Some(Err)` into the summarizer's
`topology_unavailable` argument, covered by the wiring test next.

### Wiring test (`check_declared_disks` direct, closes the previously-wrong branch)

The nine pure tests prove the summarizer and `reconcile_with_live_pool` behave, but none
proves `check_declared_disks` actually maps `ctx.pool_state == Some(Err(_))` into the
summarizer's `topology_unavailable` argument -- the exact branch that was wrong before
this revision. A regression there would keep passing every pure test *and* the VM test (a
VM cannot induce a mounted-pool `btrfs filesystem show` failure). Add one focused
wiring test that drives `check_declared_disks` with a seeded `Some(Err)` pool state.

Feasible because a non-existent by-id classifies as `Missing` via the real
`std::fs::metadata` gate without any block device or runner call, so the topology branch
is exercised deterministically:

- `check_declared_disks_warns_when_live_topology_unavailable` -- template off
  `metadata_pressure_with_cached_pool_state` (doctor.rs:4442) for the seed pattern:
  - `save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/does-not-exist", None)])`
    (fixture helper at doctor.rs:1840).
  - `let mut ctx = parsed_doctor_ctx(&runner, &paths);` -- provides a parsed config so the
    `ensure_mountpoint_is_mounted` config gate passes; a default `MockRunner` is never
    consulted (`Missing` short-circuits before any cryptsetup call).
  - `ctx.mountpoint_is_mounted = Some(true);` -- short-circuits the real mount probe so
    `ensure_pool_state` is reached and finds the pre-seeded cache.
  - `ctx.pool_state = Some(Err(ProbeError::NotBtrfs { mount_point: "/mnt/storage".into(),
    fstype: "ext4".into() }));` -- a constructible `ProbeError` (probe.rs:72) with a real
    `Display`.
  - Run `check_declared_disks(&mut ctx)`; assert `status == CheckStatus::Warn`, message
    contains `could not compare declared disks to live pool`, **and** contains `disk1`
    (the missing-disk warning -- proving per-member identity classification still runs
    when topology is unavailable).

This completes wiring coverage of all three `LiveTopology` branches: `Online -> Offline`
and `Offline` (unmounted) are covered by the VM subtests below; `Unavailable` is covered
here.

### VM test (focused): `tests/cli/braid-doctor-offline-member.{py,nix}` + `flake.nix`

A new focused test (idiomatic here -- see the existing `braid-doctor-uuid-swap`,
`braid-doctor-foreign-luks-uuid`, `braid-doctor-beep`). It proves the wiring the unit
tests cannot: that `check_declared_disks` resolves `LiveTopology::Online` when
mounted+probed and `LiveTopology::Offline` when not, and reconciles members accordingly.

Assemble from proven pieces:
- Harness/`.nix`: copy `tests/cli/braid-doctor.nix` (real RAID1-capable VM with two
  virtio disks, cryptsetup, btrfs-progs).
- Pool setup + `add_cmd` recipe: copy from `tests/cli/braid-doctor.py:182-219`
  (`braid add disk1`, `braid add disk2`, fast pbkdf args).
- Degraded/offline recipe: mirror `tests/cli/braid-status-rust.py:152-185` (mount only
  `/dev/mapper/braid-disk1 -o degraded` so disk2 is present + LUKS-verified but absent
  from the live pool).
- Register in `flake.nix` alongside the other `braid-doctor-*` checks (~lines 282-338);
  preamble per `docs/dev/testing.md`.

Subtests (Intent / Why / Scenario preamble per Test Conventions):

- **Mounted, all assembled -> ok.** After RAID1 setup, `braid doctor --json` ->
  `checks["declared_disks"]["status"] == "ok"`. (Regression: a fully-healthy mounted
  pool is not falsely flagged offline.)
- **Mounted, one member dropped -> warn + offline wording.** After degraded remount,
  `braid doctor --json` -> `checks["declared_disks"]["status"] == "warn"`; message
  contains `disk2` and `not in the live pool`. Warn keeps overall exit 0, so
  `machine.succeed` is fine.
- **Pool offline -> ok (unchanged).** Unmount entirely (members still present, headers
  readable), `braid doctor --json` -> `checks["declared_disks"]["status"] == "ok"`.
  Pins that offline-pool doctor behavior does not change.

Alternative considered and rejected: extending `tests/cli/braid-doctor.py`. Its fixed
sequence mutates data profiles (single-convert balance) and then corrupts disk1's header
before shutdown; inserting a degraded remount mid-sequence requires a fragile
re-`unlock`/remount restore so the later profile/corruption subtests still pass. A
focused test avoids that ordering coupling.

## Docs

- `docs/commands/doctor.md` -- the `declared_disks` row (~line 72). Extend the
  description and the Warn list:
  - "...its live LUKS UUID matches the `pool.json` key, **and (when the pool is mounted)
    is assembled into the live btrfs pool**."
  - Add to Warn: "...**or is present and identity-verified but not assembled into the
    live pool (`offline`); or the pool is mounted but its live topology could not be
    probed to verify assembly**". Leave Fail (UUID mismatch) unchanged.
- `docs/design/decisions/024-luks-uuid-identity.md` (`## Offline Disk State`) -- add one
  sentence that `braid doctor`'s `declared_disks` now also surfaces an offline member as
  a cause-neutral **Warn** (never Fail; Fail stays reserved for UUID mismatch), and warns
  rather than claiming health when the pool is mounted but its topology cannot be probed.
  Add the new doctor unit + VM tests to its "Tests that enforce this" list. This keeps the
  architecture authority in sync (per AGENTS.md: behavior changes update the decision
  docs).
- No `README.md` change (doctor severity nuance is reference-level, not cookbook).

## Verification gate

- `just test-rust` -- new unit tests pass; existing `declared_disks` summarizer tests
  still pass after the mechanical `, None` call-site update (no behavior change).
- `just test-vm braid-doctor-offline-member` -- new focused VM test passes. Also run
  `just test-vm braid-doctor braid-status-rust` to confirm no regression in the sibling
  doctor/status integration tests.
- `nix develop .#docs -c mdbook build docs` -- docs build + cross-link check pass
  (`docs/commands/doctor.md` and decision 024 touched; no new links added).

## Out of scope

- No change to status/TUI rendering or the deliberate no-`Action:` decision from
  `ffcdc222` -- doctor mirrors that cause-neutral posture (descriptive message, no
  remedy).
- No mapper-name identity inference (decision 024 forbids it); the cross-check is
  UUID-set membership only.
- No new membership-layer helper; no migration/back-compat (braid is unreleased).
- No change to any other doctor check or to pool-offline behavior of existing checks.
