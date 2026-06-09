# Plan: pin UUID-mismatch detection on a non-first pool member

## Context

`plan_open_pool_inner` (`cli/src/mount.rs`) probes pool members in
`iter_by_name()` order. For each healthy member it pushes a
`ProbeEvent::DiskAvailable` into the shared `events` vec; on the first LUKS
UUID mismatch it `return`s `MountError::Failed(...)` immediately
(`cli/src/mount.rs:258-265`) -- before the `mapper_open` branch and before
the degraded gate (`:293-298`). The `events` vec is untouched by that
return, so events accumulated for earlier members survive on the error path.

The behavior is correct but **unpinned for any member after the first**.
All three existing mismatch tests -- `mount_luks_uuid_mismatch_closed`,
`_already_open`, `_refused_even_with_allow_degraded` -- mismatch `disk1`,
the first name in iteration order, so the early return fires with
`events == []`. The two events/notes-preservation tests
(`plan_open_pool_emits_events_before_degraded_refused` in mount.rs,
`plan_unlock_preserves_notes_on_degraded_refused` in unlock.rs) only cover
the **DegradedRefused** path, never the mismatch path. The
`unlock-uuid-mismatch.py` VM test reformats `disk2` but only asserts the
error names `disk2`/the UUIDs -- never that `disk1`'s probe row precedes it.

So two promises from ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md`)
and `docs/commands/unlock.md` are untested:

1. **Position-independence** -- the mismatch is a hard error regardless of
   which member carries it (the loop keeps probing past healthy members).
2. **Probe context precedes the refusal** -- a healthy member's `found`
   event, already pushed, survives on the mismatch `Err` path.

A regression that routed the mismatch through the `missing` vector + degraded
gate, or that dropped already-pushed events on the mismatch return, would
pass every current test. This plan closes the gap at the unit and VM layers
and removes dead boilerplate the new test must not inherit.

## Change 1 (core): mount-level regression test

Add to `cli/src/mount.rs` `mod tests`, immediately after
`plan_open_pool_emits_events_before_degraded_refused` (its mechanistic
sibling -- both call `plan_open_pool` directly and assert on
`report.events` / `report.result`, unlike the `mount_luks_uuid_mismatch_*`
family which uses `open_and_mount_for_test` and discards events). The name
keeps the `plan_open_pool_emits_events_before` prefix and the grep-able
`uuid_mismatch` token so the mismatch family is still discoverable.

Reuses existing fixtures unchanged: `two_disk_membership()` (disk1->`1111`,
disk2->`2222`), `base_two_disk_runner()` (both probe matching, mappers
closed), `luks_uuid_ok()`, `mount_fs()`, `test_config()`,
`mock_virtio_backing_path_resolver()`. The mismatch is driven purely by
overriding **disk2's** probe UUID -- no membership surgery needed.

```rust
/// Intent: A LUKS UUID mismatch on a *later* membership member (by name
/// order) is still caught as a hard `MountError::Failed`, and the probe
/// events already accumulated for the healthy members ahead of it survive
/// on the error path.
///
/// Why: Every other mismatch test (`mount_luks_uuid_mismatch_closed`,
/// `_already_open`, `_refused_even_with_allow_degraded`) mismatches `disk1`
/// -- the first name in `iter_by_name` order -- so the early
/// `return Err(Failed)` in `plan_open_pool_inner` fires before any
/// `DiskAvailable` event is pushed. That leaves two promises from ADR 024
/// and docs/commands/unlock.md unpinned: the mismatch is position-
/// independent (the loop keeps probing past healthy members), and operator
/// probe context renders before the refusal. A regression that routed the
/// mismatch through the `missing` vector + degraded gate, or that dropped
/// already-pushed events on the mismatch return, would pass every existing
/// test but fail here.
///
/// Scenario: 2-disk RAID1. disk1 is healthy and probed first (mapper closed
/// -> classified Available). disk2's device now reports a UUID that differs
/// from its stored membership key (swapped/reformatted drive). The plan must
/// fail with the UUID-mismatch error, not a degraded refusal, and must still
/// carry disk1's "found" event.
#[test]
fn plan_open_pool_emits_events_before_uuid_mismatch_on_later_member() {
    let config = test_config();
    let membership = two_disk_membership();
    let fs = mount_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
    ]);

    // disk1 probes healthy (stored 1111 == probed 1111 from base runner).
    // Override only disk2's probe UUID so it mismatches its stored 2222
    // membership key -- the mismatch now lands on the *second* member.
    let (uuid2_req, uuid2_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk2",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
    );
    let runner = base_two_disk_runner().with_output(uuid2_req, uuid2_out);

    let report = plan_open_pool(
        &runner,
        &fs,
        &config,
        &membership,
        crate::test_fixtures::mock_virtio_backing_path_resolver(),
        false,
        "unlock",
    );

    // disk1's healthy probe event survives on the mismatch Err path, and is
    // the only event: disk2 returns before pushing its own.
    assert_eq!(
        report.events,
        vec![ProbeEvent::DiskAvailable {
            name: "disk1".to_owned()
        }],
        "disk1's found event must precede and survive the disk2 mismatch, got: {:?}",
        report.events,
    );

    let err = report
        .result
        .expect_err("UUID mismatch on a later member must still fail");
    assert!(
        matches!(&err, MountError::Failed(_)),
        "mismatch must be a hard Failed, not DegradedRefused, got: {err:?}",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("LUKS UUID mismatch"),
        "error should be the UUID-mismatch refusal, got: {msg}",
    );
    assert!(
        msg.contains("disk2"),
        "mismatch must name the later member, got: {msg}",
    );
}
```

The `assert_eq!` on the exact one-element vec pins all three guarantees at
once: position-independence (disk2's mismatch is reached and returned),
event preservation (disk1's event is present on the `Err` path), and that
disk2 contributed no event (it returned before pushing).

## Change 2: pin the ordering at the VM layer

In `tests/cli/unlock-uuid-mismatch.py`, inside the existing
"UUID mismatch: reformatted disk2 detected and rejected" subtest, after the
current `assert "disk2" in ret[1]` (the subtest already reformats disk2 and
captures `ret = machine.execute(unlock_cmd() + " 2>&1")`, so this is a
near-zero-cost addition -- no new VM boot):

```python
# The healthy disk1 probe-OK row must render before the mismatch error,
# proving probe context precedes the refusal (ADR 024, unlock.md) and that
# the mismatch on a *later* member is caught (disk1 is classified first).
# Anchor on the full rendered row, not bare "disk1": that token also occurs
# in by-id device paths and remediation text, so it would not prove the
# probe row itself rendered. close_all() at the top of this subtest closes
# braid-disk1, so disk1 probes closed -> classified Available -> "found"
# (an open mapper would render "already open" instead). The "disk <name>:
# <message>" body is pinned by the Rust test
# render_probe_events_formats_mixed_probe_result; stderr is uncolored under
# capture, and color (when on) wraps only the [ok] tag, never the body.
probe_row = "disk disk1: found"
assert probe_row in ret[1], (
    f"Expected healthy disk1 probe-OK row {probe_row!r} in output, got: {ret[1]}"
)
assert ret[1].index(probe_row) < ret[1].index("LUKS UUID mismatch"), (
    f"disk1 probe row must precede the mismatch error, got: {ret[1]}"
)
```

`probe_row` is the exact rendered body (`disk <name>: <message>`), produced
only as disk1's OK note, so both the membership assert and the `.index()`
ordering assert are unambiguous -- bare `disk1` (which also appears in by-id
paths) would not prove the probe row rendered.

## Change 3: remove dead boilerplate from the 3 existing mismatch tests

In `cli/src/mount.rs`, `mount_luks_uuid_mismatch_closed`,
`_already_open`, and `_refused_even_with_allow_degraded` each open with a
`remove_by_uuid`/`insert` block that removes `disk1` at UUID `1111...` and
re-inserts the same member at the same `1111...` key -- a verified no-op
(the mismatch is driven entirely by the `with_output(uuid1 -> ffff...)`
probe override). Replace the whole block in each:

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

with:

```rust
let membership = two_disk_membership();
```

If the compiler then flags an unused `LuksUuid` import in the mount.rs test
module, drop it. Behavior is unchanged -- the three tests must still pass.

## Critical files

- `cli/src/mount.rs` -- add Change 1's test; apply Change 3 to the three
  existing mismatch tests. (Production code at `:258-298` is unchanged.)
- `tests/cli/unlock-uuid-mismatch.py` -- add Change 2's two assertions.

No production code changes; no doc changes (ADR 024 and `unlock.md` already
state the promises this pins).

## Verification

1. **New + existing unit tests pass:**
   ```
   cargo test --manifest-path cli/Cargo.toml --lib uuid_mismatch
   ```
   Confirms the new `plan_open_pool_emits_events_before_uuid_mismatch_on_later_member`
   passes and the three reworded mismatch tests still pass. Then the full
   lane: `just test-rust`.

2. **New test fails for the right reason (TDD sanity):** temporarily change
   `cli/src/mount.rs:258` from `return Err(...)` to push the mismatch into
   `missing` instead -- the new test must fail on the `matches!(Failed)` /
   `assert_eq!(events, ...)` assertion (it would become `DegradedRefused` or
   lose disk1's event). Revert.

3. **VM test passes with the new ordering assertion:**
   ```
   nix build .#checks.aarch64-darwin.unlock-uuid-mismatch -L
   ```
   (confirm the exact check name via `nix flake show` if it differs).

4. **Lints clean:** `just clippy`.

## Implementation notes

- Change 3 was already landed by commit `8320c892` ("test(cli): drop the
  no-op uuid-rekey ceremony in the mount mismatch tests") before this plan
  was implemented, so it is a no-op against the tree: the three
  `mount_luks_uuid_mismatch_*` tests already open with a bare
  `let membership = two_disk_membership();` and the `LuksUuid` test-module
  import is already trimmed. Only Change 1 (mount.rs test) and Change 2
  (VM-test assertions) were applied here; no production code changed.
