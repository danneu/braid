# Plan: fix misleading "relabel or detach" remediation for duplicate-LUKS-UUID errors

## Context

`DiscoverError::DuplicateUuid` and `AddError::DuplicateUuid` both tell the
operator to "relabel or detach one before retrying" when two physically
distinct disks share a single LUKS UUID (the `dd`-cloned-disk hazard).
Relabel rewrites the `braid-<name>` cryptsetup label, which is a
different LUKS header field from the UUID. Following the message
literally - relabeling one of the two cloned disks - will trip the same
duplicate-UUID guard on the next `braid discover` (or `braid add`),
costing the operator a recovery cycle.

The user-facing manual at `manual/commands/discover.md:78` already says
"detach the cloned or unintended disk before retrying" only. The sibling
error `ReplaceError::DuplicateUuid` (`cli/src/replace.rs:59`) already
says "detach the conflicting disk before retrying". Code, manual, and
siblings are out of sync only on the two `DuplicateUuid` variants flagged
above; this plan brings them into line.

Two additional pivots from the original finding's scope:

1. `add.rs:51-57` explicitly documents `AddError::DuplicateUuid` as
   mirroring `DiscoverError::DuplicateUuid`, so both messages must change
   together or they will re-diverge.
2. `AddError::DuplicateUuid` is raised from three branches inside
   `assert_target_uuid_unique` (`add.rs:1894-1944`): cross-add-targets,
   membership-collision, and live-pool collision. The current prefix
   `"duplicate LUKS UUID across add targets:"` is only accurate for the
   first branch -- a single-target `braid add` that collides with an
   existing pool member or open mapper shows the wrong context. This
   plan rewords the prefix to be context-neutral
   (`"duplicate LUKS UUID:"`), matching the discover variant.

Intended outcome: operators who hit any duplicate-UUID refusal in
`discover` or `add` see the correct, actionable remediation regardless of
which branch fired; tests pin the exact new clause so a regression to
"relabel or detach" (or to the over-narrow "across add targets" prefix)
fails loudly.

## Scope

In scope:

- Reword `DiscoverError::DuplicateUuid` message: replace "relabel or
  detach one before retrying" with "detach the cloned or unintended
  disk before retrying".
- Reword `AddError::DuplicateUuid` message: same remediation change,
  PLUS drop the "across add targets" prefix qualifier so the message is
  accurate for all three branches of `assert_target_uuid_unique`
  (in-flight collision, membership collision, live-pool collision).
- Update the `AddError::DuplicateUuid` variant doc comment
  (`add.rs:51-57`) so the internal description matches the
  now-branch-neutral message (currently describes only the
  cross-add-targets branch).
- Fix the same misleading phrasing in the discover doc comment at
  `cli/src/discover.rs:30`.
- Extend / update the two existing Display-string tests to pin the
  exact new remediation clause ("detach the cloned or unintended disk
  before retrying") so a regression to "relabel or detach" -- which
  also contains the substring "detach" -- fails the assertion.

Out of scope:

- `DiscoverError::LabelCollision` (`cli/src/discover.rs:21`) - "relabel
  or detach" is correct for the label-collision case; relabel genuinely
  resolves that conflict.
- `manual/commands/discover.md` - L65 and L78 are already correct.
- `plans/impl/2026-05-04-discover-label-collision.md` and
  `plans/impl/2026-05-12-luks-uuid-as-identity/plan.md` - historical
  plan records; per repo convention, impl/ plans are not retroactively
  edited.
- VM tests under `tests/` - `replace-new-in-pool-guard.py:69` is the
  only VM-side pin and it does not match on the misleading half.
- The membership-collision branch test at `add.rs:7568-7630` - it does
  not pin the Display body, only structural fields.

## Changes

### 1. `cli/src/discover.rs` - DuplicateUuid message

Replace the `#[error(...)]` attribute on `DuplicateUuid` (at L34-36).

Old:

```rust
#[error(
    "duplicate LUKS UUID: braid-{name1} ({path1}) and braid-{name2} ({path2}) share UUID {uuid} -- relabel or detach one before retrying (this typically indicates a dd-cloned disk)"
)]
```

New:

```rust
#[error(
    "duplicate LUKS UUID: braid-{name1} ({path1}) and braid-{name2} ({path2}) share UUID {uuid} -- detach the cloned or unintended disk before retrying (this typically indicates a dd-cloned disk)"
)]
```

Also update the doc comment at `cli/src/discover.rs:30` so internal
intent matches the operator-facing message:

Old: `/// labels so the operator can pick which one to relabel or detach.`
New: `/// labels so the operator can pick which one to detach.`

### 2. `cli/src/add.rs` - DuplicateUuid message

Replace the `#[error(...)]` attribute on `AddError::DuplicateUuid` (at
L58-60). Two changes in one edit:

1. Remediation: "relabel or detach one before retrying" -> "detach the
   cloned or unintended disk before retrying".
2. Prefix: drop "across add targets" so the message is accurate for the
   membership-collision and live-pool-collision branches of
   `assert_target_uuid_unique` (`add.rs:1913-1942`), not just the
   in-flight branch.

Old:

```rust
#[error(
    "duplicate LUKS UUID across add targets: braid-{name1} ({by_id1}) and braid-{name2} ({by_id2}) share UUID {uuid} -- relabel or detach one before retrying (this typically indicates a dd-cloned disk)"
)]
```

New:

```rust
#[error(
    "duplicate LUKS UUID: braid-{name1} ({by_id1}) and braid-{name2} ({by_id2}) share UUID {uuid} -- detach the cloned or unintended disk before retrying (this typically indicates a dd-cloned disk)"
)]
```

After this edit `DiscoverError::DuplicateUuid` and
`AddError::DuplicateUuid` render with identical message shapes (only
the field names `path*` vs `by_id*` differ, which is intentional and
already reflects the typed surfaces).

Also update the variant doc comment at `cli/src/add.rs:51-57` so the
internal guidance matches the now-branch-neutral message. The current
text describes only the cross-add-targets branch ("Two adoption
targets in a single `braid add` invocation point at distinct by-id
paths but share a LUKS UUID"), which is stale once the variant covers
membership and live-pool collisions too.

Old:

```rust
/// Two adoption targets in a single `braid add` invocation point at
/// distinct by-id paths but share a LUKS UUID -- the dd-cloned-disk
/// case. Raised before journal write, before any
/// `CryptsetupLuksFormat`, and before any `PoolMembership::insert`,
/// so the operator-facing message names both by-id paths and
/// suggests cloning as the typical cause. Mirrors
/// `DiscoverError::DuplicateUuid`.
```

New:

```rust
/// Pre-journal-write refusal: a target's LUKS UUID collides with
/// another in-flight add target, an existing pool member, or a UUID
/// observed in the live `pool.devices` set. Raised by
/// `assert_target_uuid_unique` before journal write, before any
/// `CryptsetupLuksFormat`, and before any `PoolMembership::insert`,
/// so the operator-facing message names both `(name, by_id)` pairs
/// and suggests cloning as the typical cause. Mirrors
/// `DiscoverError::DuplicateUuid`.
```

### 3. `cli/src/discover.rs:1422-1429` - extend test

In `discover_duplicate_uuid_surfaces_friendly_error`, add a positive
pin on the full new remediation clause before the existing `dd-cloned
disk` assertion. Keep the surrounding assertions unchanged.

Add:

```rust
assert!(
    msg.contains("detach the cloned or unintended disk before retrying"),
    "missing detach remediation clause: {msg}"
);
```

Note: a bare `contains("detach")` is NOT sufficient -- the current
buggy string "relabel or detach one before retrying" already contains
"detach", so it would pass against the unfixed code. The full clause is
present only in the new wording, so the assertion fails before the fix
and passes after.

### 4. `cli/src/add.rs:7268-7273` - update existing pin

In the cross-add-targets duplicate-UUID test (the one that asserts
`body.contains(...)` against the Display string), two changes:

1. Drop "across add targets" from the pinned prefix so it matches the
   reworded message.
2. Add a sibling assertion pinning the full new remediation clause
   (same reason as the discover test above).

Replace the existing `body.contains(...)` block:

Old:

```rust
assert!(
    body.contains(
        "duplicate LUKS UUID across add targets: braid-diska (/dev/disk/by-id/usb-CLONE-AAAA) and braid-diskb (/dev/disk/by-id/usb-CLONE-BBBB) share UUID 55555555-5555-5555-5555-555555555555"
    ),
    "Display must match the pinned wording: {body}"
);
```

New:

```rust
assert!(
    body.contains(
        "duplicate LUKS UUID: braid-diska (/dev/disk/by-id/usb-CLONE-AAAA) and braid-diskb (/dev/disk/by-id/usb-CLONE-BBBB) share UUID 55555555-5555-5555-5555-555555555555"
    ),
    "Display must match the pinned wording: {body}"
);
assert!(
    body.contains("detach the cloned or unintended disk before retrying"),
    "missing detach remediation clause: {body}"
);
```

The first assertion now fails before the prefix change (because the old
prefix contains the dropped "across add targets" qualifier) and the
second fails before the remediation change. Together they pin both
edits independently.

## Critical files

- `cli/src/discover.rs` - error variant + doc comment + unit test.
- `cli/src/add.rs` - mirrored error variant + unit test.

## Sibling sites already correct (referenced for tone)

- `cli/src/replace.rs:59` - `ReplaceError::DuplicateUuid`: "detach the
  conflicting disk before retrying".
- `cli/src/replace.rs:80` - `ReplaceError::NewTargetUuidMismatchAtOpen`:
  "detach the foreign disk and retry".
- `manual/commands/discover.md:78` - "detach the cloned or unintended
  disk before retrying" (the exact phrasing being adopted).

## Verification

- **Before applying fix:** `just test-rust` must fail with the new
  assertions against the current code (sanity check that the
  assertions actually pin the buggy wording, not a synonym that
  overlaps).
  - `discover_duplicate_uuid_surfaces_friendly_error` fails because the
    current message has "relabel or detach one before retrying", not
    "detach the cloned or unintended disk before retrying".
  - The cross-add-targets test fails twice over: the new prefix
    assertion rejects "across add targets", and the new remediation
    assertion rejects "relabel or detach".
- **After applying fix:** `just test-rust` passes.
- `grep -rn "relabel or detach" cli/src/` - after the change, the only
  remaining hit must be the `LabelCollision` variant at
  `cli/src/discover.rs:21` (and its doc comment context). Any hit on a
  `DuplicateUuid` site is a regression.
- `grep -rn "across add targets" cli/src/` - after the change, must
  return zero hits (the error definition and the test were the only
  two sites).
- `cargo build` (via `just test-rust`) - confirms `thiserror` derive
  picks up the new message format strings without macro errors.
- No VM test changes expected; `just test-vm` is not required for this
  change. (Run a quick smoke if desired: `tests/cli/replace-new-in-pool-guard.py`
  only matches on "duplicate LUKS UUID" + "already present in
  membership" -- unaffected.)
