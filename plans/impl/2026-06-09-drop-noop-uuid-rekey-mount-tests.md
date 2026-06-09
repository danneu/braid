# Plan: delete the no-op UUID-rekey ceremony in the three `mount_luks_uuid_mismatch_*` tests

## Context

Three tests in `cli/src/mount.rs` -- `mount_luks_uuid_mismatch_closed`,
`mount_luks_uuid_mismatch_already_open`, and
`mount_luks_uuid_mismatch_refused_even_with_allow_degraded` -- each open with a
block that removes disk1 from the membership and re-inserts the *same*
`DiskMember` value under the *same* `11111111-...` UUID key:

```rust
let mut membership = two_disk_membership();
let disk1_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
let disk1 = membership
    .remove_by_uuid(&disk1_uuid)
    .expect("disk1 fixture member");
membership
    .insert(
        LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
        disk1,
    )
    .expect("replace disk1 fixture UUID");
```

This is a provable no-op. `DiskMember` carries no UUID -- identity is purely the
map key (`membership.rs#DiskMember`, doc comment: *"the value itself does NOT
carry the LUKS UUID"*). `remove_by_uuid` is `BTreeMap::remove` and `insert`
re-adds under the identical key (`membership.rs#PoolMembership::insert`,
`#remove_by_uuid`), so the membership ends byte-for-byte identical to
`two_disk_membership()`. The actual mismatch is induced entirely by the
`luks_uuid_ok("/dev/disk/by-id/virtio-disk1", "ffffffff-...")` mock override,
which makes the probed header report `ffffffff-...` while the membership key
stays `11111111-...`; the production gate `expected_uuid != uuid`
(`mount.rs:258`, comparing the membership key against the probed header) then
fires.

The block is residue from commit `9c23a15a` ("finish luks uuid identity
migration"), back when `DiskMember` still carried a `luks_uuid` field. Its
`.expect("replace disk1 fixture UUID")` message implies a rekey that no longer
happens, which misleads future maintainers and invites copy-paste of the dead
ceremony into new tests.

**Outcome:** remove the dead ceremony so each test reads as what it is -- a
fixed membership plus a mock override that creates the mismatch -- with no
behavior change and no new compiler/clippy warnings.

## Scope note (what is NOT changing)

- The look-alike `remove_by_uuid` + `insert` pairs in `recover.rs:15265` and
  `recover.rs:17597` are **not** touched. They mutate the value before
  re-inserting (`devid: Some(2), ..new_member`; `new_member.by_id = ...`), so the
  remove is a required step to edit a value in the fail-closed `LuksUuidMap`
  (`insert` rejects an existing key). They are legitimate, not no-ops.
- No helper extraction / test-dedup. The three tests intentionally vary
  (`already_open` builds its own `MockRunner` with a mapper-open seed instead of
  `base_two_disk_runner()`; `refused_even_with_allow_degraded` flips
  `allow_degraded`). Their building blocks already live centralized in
  `cli/src/test_fixtures/mount.rs`. Collapsing them is out of scope for this
  cleanup.

## Change

### File: `cli/src/mount.rs`

**1. In each of the three tests**, delete the `disk1_uuid` binding plus the
`remove_by_uuid` + `insert` block, and demote the binding to immutable. Each
test's opening goes from the snippet above to:

```rust
let membership = two_disk_membership();
```

`membership` is only ever passed as `&membership` to `open_and_mount_for_test`
in these tests, so dropping `mut` is correct. The `// Override base's disk1 UUID
seed ...` comment above the `luks_uuid_ok(..., "ffffffff-...")` override (tests 1
and 3) stays -- it accurately documents the real mismatch mechanism.

Representative locations (the same edit, three times):
- `mount_luks_uuid_mismatch_closed` -- `mount.rs:2120-2131`
- `mount_luks_uuid_mismatch_already_open` -- `mount.rs:2183-2194`
- `mount_luks_uuid_mismatch_refused_even_with_allow_degraded` -- `mount.rs:2266-2277`

**2. Trim the now-orphaned import.** `LuksUuid` (the type) is imported in the
test module at `mount.rs:861` and used *only* by these three tests. After the
deletions it is unused, so change:

```rust
use crate::types::{ByIdPath, LuksUuid, MountPoint};
```

to:

```rust
use crate::types::{ByIdPath, MountPoint};
```

`ByIdPath` (5 other uses) and `MountPoint` (16 other uses) stay. This step is the
one detail beyond the original finding's "delete the blocks" -- without it,
`cargo`/clippy emits an unused-import warning (non-fatal here -- no
`-D warnings` / `#![deny(warnings)]` in the crate or CI -- but a clean edit
removes it).

## Verification

1. `just test-rust` (`cargo test --lib ...`) -- the three
   `mount_luks_uuid_mismatch_*` tests still pass. Behavior is preserved because
   the post-deletion membership is identical to the no-op version (disk1 under
   `11111111-...`, disk2 under `22222222-...`) and the `ffffffff-...` mock
   override is untouched, so the `expected_uuid != uuid` gate still fires and the
   `contains("111111")` / `contains("ffffffff")` / remediation assertions still
   hold.
2. `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) -- clean,
   with no `unused_variables` (`disk1_uuid`), `unused_mut` (`membership`), or
   `unused_imports` (`LuksUuid`) warnings introduced.

## Critical files

- `cli/src/mount.rs` -- the only file edited (three test bodies + one import line).
- `cli/src/membership.rs` -- reference only (`DiskMember` no-UUID model, `insert`
  / `remove_by_uuid`), confirms the no-op.
- `cli/src/test_fixtures/mount.rs` -- reference only (`two_disk_membership`,
  `base_two_disk_runner`, `luks_uuid_ok`), confirms what the mismatch override does.
