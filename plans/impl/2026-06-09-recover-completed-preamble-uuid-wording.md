# Plan: fix stale "by-id membership resolution" wording in recover-remove-missing-completed preambles

## Context

The `recover-remove-missing-completed` test's "Why it exists" preamble claims it
pins **"real by-id membership resolution."** That is pre-ADR-024 vocabulary. Under
[ADR 024 (LUKS UUID Is Disk Identity)](../../docs/design/decisions/024-luks-uuid-identity.md),
by-id is a hardware address ("setup and repair only"), never the membership key.

What this test actually exercises (the *committed* recover path, where btrfs has
already removed the missing device before `braid recover` runs, so
`pool.missing_devids` is empty and `by_devid` never fires):

- **UUID-keyed membership resolution.** `recover_membership_matching_expected`
  joins each surviving live device to the journal's target membership by LUKS UUID
  (`expected.by_uuid(&dev.luks_uuid)`, `cli/src/recover.rs:1661`).
- **by-id re-resolved from the live device.** Each recovered member's `by_id`
  field is re-derived from the live backing device via
  `resolve_by_id_for_underlying` (`cli/src/recover.rs:113`), and the test asserts
  those values at `recover-remove-missing-completed.py:160-164`. This is the value
  field the VM run uniquely covers -- the unit tests mock `ByIdResolver`.

The fix reframes the stale clause to name both real invariants the test guards, in
ADR 024's own "UUID-keyed" language. Comment-only; no behavior change.

The finding's proposed "devid->UUID membership resolution" was rejected: for this
*completed* path `by_devid` never fires, so the join is `by_uuid` -- "devid->UUID"
would be a fresh inaccuracy.

## Changes

Two files, identical clause swap, each preserving its local surrounding phrasing.
Replace only the stale clause -- leave Intent/What, Scenario, and the other listed
items (`degraded mount`, `btrfs missing-device probing/removal`,
`journal`/`pending-op cleanup`) untouched.

### 1. `tests/cli/recover-remove-missing-completed.py` (the "Why it exists" block, lines 6-8)

Before:
```
# Why it exists: Unit tests cover the dispatcher with mocked pool states, but
# this pins the VM integration path: degraded mount, real btrfs missing-device
# probing, real by-id membership resolution, and journal cleanup.
```

After:
```
# Why it exists: Unit tests cover the dispatcher with mocked pool states, but
# this pins the VM integration path: degraded mount, real btrfs missing-device
# probing, UUID-keyed membership resolution with by-id re-resolved from the
# live backing device, and journal cleanup.
```

### 2. `tests/cli/recover-remove-missing-completed.nix` (the "Why" block, lines 6-7)

Before:
```
# Why: This pins the VM path for degraded mount probing, real btrfs missing
# device removal, by-id membership resolution, and pending-op cleanup.
```

After:
```
# Why: This pins the VM path for degraded mount probing, real btrfs missing
# device removal, UUID-keyed membership resolution with by-id re-resolved from
# the live backing device, and pending-op cleanup.
```

## Scope

Complete at these two files. `grep -rn "by-id membership\|by_id membership"` over the
repo returns only these two preamble hits; no docs/ADRs carry the phrase. (Do not
widen the pattern to `by-id resolution` -- that legitimately matches `cli/src/recover.rs`
and prior `plans/impl/` docs, and is not stale.) The test
*body* is already correct (UUID-keyed `pool["disks"]` map, `member["name"]` /
`member["by_id"]` asserts) and is not touched.

## Verification

Comment-only change in test preambles, so no test run gates it. Confirm accuracy
and that no stale phrasing remains:

1. `grep -rn "by-id membership\|by_id membership\|devid->UUID" tests/ docs/` -> no hits.
2. Re-read both preambles: every listed item maps to a real step the test performs
   (degraded mount at `.py:102`; missing-device probing at `.py:59-64,106`;
   UUID join at `recover.rs:1661`; by-id re-resolution asserted at `.py:160-164`;
   journal cleanup at `.py:151`).
3. Wording is ASCII (no em-dash/arrow/curly quotes), consistent with project
   convention.
