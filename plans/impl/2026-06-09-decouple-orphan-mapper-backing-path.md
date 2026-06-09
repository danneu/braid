# Plan: decouple `with_orphan_mapper`'s synthetic backing path from the mapper name

## Context

A review finding (Low / Simplicity) flagged the test helper `with_orphan_mapper`
in `cli/src/lock.rs`:

```rust
const ORPHAN_UUID: &str = "00000000-0000-0000-0000-0000000002ff";

fn with_orphan_mapper(runner: MockRunner, mapper: &str) -> MockRunner {
    let device = format!("/dev/disk/by-id/{mapper}");      // <- /dev/disk/by-id/braid-ccc
    runner.with_mapper_open(mapper, &device, ORPHAN_UUID)
}
```

It synthesizes a backing-device path *from the mapper name* (e.g.
`/dev/disk/by-id/braid-ccc`). This is inert -- the path is only used to key the
mock's `cryptsetup luksUUID` response, and orphan identity is decided purely by
the non-member UUID -- but the construction is confusing for two reasons:

1. It stuffs a mapper-style token into a `by-id`-style path. A real LUKS backing
   device is never `/dev/disk/by-id/<mapper>`; it is `ata-...`/`wwn-...`/`/dev/vdX`.
2. Deriving a `by-id` (hardware/identity) handle from a runtime handle (mapper
   name) superficially resembles the name->identity coupling
   [ADR-024](../../docs/design/decisions/024-luks-uuid-identity.md) forbids, so it
   invites a false-positive review flag (this finding is one).

It is also the **lone outlier**: every other `with_mapper_open` caller in the repo
passes a backing path decoupled from the mapper name (`/dev/disk/by-id/a`,
`/dev/vda`, `/dev/vdb`, ...). The convention-consistency case is what lifts this
above "too trivial to bother."

Intended outcome: the helper passes a fixed, obviously-synthetic backing path that
cannot be misread as identity, bringing it in line with its siblings, with a short
comment so the same finding is not re-raised later.

## Verification that the change is behavior-preserving

Traced during investigation:

- `with_mapper_open` (`cli/src/cmd.rs#with_mapper_open`) embeds `underlying` in two
  places: the `device:` line of the `cryptsetup status` stub and the key for the
  `cryptsetup luksUUID` stub. The value only needs the two to agree and to resolve
  to `ORPHAN_UUID`.
- `classify_candidate_mapper` (`cli/src/lock.rs#classify_candidate_mapper`) parses
  the backing path solely to issue `luksUUID`, classifies by the returned UUID, and
  for an orphan takes the display name from `name_from_mapper(mapper)` -- never the
  backing path. (`uuid_scanned_fallback_malformed_mapper_with_uuid_is_orphan`, which
  asserts the orphan name `..foo` comes from mapper `braid-..foo`, proves this.)
- `MockRunner.outputs` is a `HashMap` (`cli/src/cmd.rs`, `with_output` does
  `insert`). In the only multi-orphan test
  (`cmd_lock_with_empty_membership_closes_observed_orphan_mappers`, two nested
  `with_orphan_mapper` calls), a single shared constant makes both mappers register
  the `luksUUID` mock under the same key -- a harmless overwrite with the identical
  `ORPHAN_UUID` value. Per-mapper `status` mocks stay distinct (keyed by mapper), and
  orphan names still come from the mappers, so assertions are unchanged. No per-call
  path distinctness is required.
- No test references the old derived paths (`/dev/disk/by-id/braid-ccc`, etc.)
  outside the helper, so the change is self-contained.

## The change

File: `cli/src/lock.rs` (test module, near the `*_UUID` consts at the top of
`mod tests`).

Add a paired, self-documenting constant beside `ORPHAN_UUID` and drop the
mapper-derived `format!`:

```rust
const ORPHAN_UUID: &str = "00000000-0000-0000-0000-0000000002ff";
// Synthetic stand-in backing device for orphan mappers: a mapper is an orphan
// because its backing LUKS UUID (ORPHAN_UUID) is non-member, not because of any
// path value. Kept decoupled from `mapper` so it is not misread as the
// name->identity coupling ADR-024 forbids.
const ORPHAN_BACKING: &str = "/dev/disk/by-id/orphan-backing";

fn with_orphan_mapper(runner: MockRunner, mapper: &str) -> MockRunner {
    runner.with_mapper_open(mapper, ORPHAN_BACKING, ORPHAN_UUID)
}
```

That is the whole fix: one new const, one comment, the `let device = format!(...)`
local removed. All ~10 existing call sites are unchanged (they pass only a mapper).

### Why this shape (and not the alternatives)

- **Fixed constant, not distinct-per-mapper.** A single `ORPHAN_BACKING` actually
  communicates the inertness *better* than a distinct path would -- it signals "the
  backing path is a throwaway; only the UUID's non-membership matters." Distinct
  paths would imply the value is load-bearing, which it is not. The multi-orphan
  overwrite is benign (above).
- **`by-id` namespace kept.** Real LUKS backing devices are `by-id` paths, and the
  lock-test siblings use `/dev/disk/by-id/a`/`b`, so `by-id` is the house style and
  is realistic. The finding's concern was the *mapper-derived* value, not `by-id`
  itself; a fixed `orphan-backing` removes the coupling without leaving the
  namespace.
- **Named const, not inline literal.** Mirrors the adjacent `ORPHAN_UUID` and makes
  the don't-care intent obvious at a glance.

## Out of scope (considered, deliberately excluded)

`cli/src/tui/probe.rs` and `cli/src/tui/model.rs` use `/dev/disk/by-id/braid-<name>`
paths (`braid-toshiba`, `braid-ironwolf`, `braid-zeta`, `braid-alpha`). These are
**not** the same pattern: there the by-id is a member's persisted hardware handle
that is genuine, load-bearing test input -- it is fed to `luksUUID`/`luksDump`
requests, mapped to live nodes (`with_path(... "/dev/vdb")`), and asserted on -- and
a recognizable name aids correlation across ~40 references. That is normal fixture
practice, not an inert value coupled for no reason, so changing it would add churn
and reduce readability without fixing a real defect. Left as-is.

## Verification

1. Targeted, fast loop -- run the lock unit tests (from the `cli/` crate dir):
   ```
   cargo test --lib 'lock::tests'
   ```
   Confirm green, paying attention to the orphan/fallback tests that exercise the
   helper: `lock_closes_orphaned_mapper`,
   `cmd_lock_with_empty_membership_closes_observed_orphan_mappers` (the multi-orphan
   overwrite case), `fallback_member_named_mapper_with_different_uuid_is_orphan`, and
   `uuid_scanned_fallback_malformed_mapper_with_uuid_is_orphan`.
2. Canonical full Rust suite:
   ```
   just test-rust
   ```
3. No fixture refresh and no NixOS VM test run is needed -- this touches only an
   in-crate mock helper, not parser fixtures or module behavior.
