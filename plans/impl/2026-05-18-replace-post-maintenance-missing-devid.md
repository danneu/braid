# Plan: pin replace post-maintenance against non-target MISSING devid

## Context

Commit `21ea1b6 fix(recover): preserve missing members in phased recovery`
landed a re-insert loop in `recover_membership_matching_expected`
(`cli/src/recover.rs:1671-1715`) that materializes devid-only members from
`target_membership` whenever the live pool reports a non-target devid in
either `pool.missing_devids` or `pool.null_underlying`. Both phased
PostMaintenance helpers -- `execute_remove_missing_post_maintenance_recovery`
and `execute_replace_post_maintenance_recovery` -- depend on that loop.

The helper arm itself is already pinned at the unit level by
`recover_membership_matching_expected_reinserts_missing_devid_member`
(`cli/src/recover.rs:10871`): it constructs a `PoolState` with
`pool.missing_devids = vec![3]`, calls the helper directly, and asserts
disk3 survives in the returned membership with `added_at` from the prior
record. A regression that dropped the `pool.missing_devids` half of the
chain would fail that test.

What the unit test does NOT cover is the composed command-boundary path:
that an interrupted `OpKind::Replace` whose journal is parked in
`PostReplaceMaintenance` actually routes through
`execute_replace_post_maintenance_recovery`, actually calls the helper,
and actually writes the recovered membership to `pool.json` -- with a
non-target btrfs `MISSING` devid in the live pool. The sibling fix
`8bc6d74 fix(recover): preserve null-underlying members in phased
recovery` landed exactly this composed coverage for the `null_underlying`
side via
`cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk`
(`cli/src/recover.rs:10631`). No equivalent test exists for the
`missing_devids` side of `Replace::PostReplaceMaintenance` -- so a wiring
regression in the post-maintenance dispatcher, the
`live_pool_matches_membership` gate, the helper call site, or the
follow-on `save_membership` / resize / journal-clear sequence would slip
past CI even with the helper unit test green.

The fix is one additive integration test that closes that symmetric
command-boundary gap.

## Scope

Add a single Rust unit test in the existing `#[cfg(test)] mod tests` block of
`cli/src/recover.rs`:

- `cmd_recover_replace_post_maintenance_preserves_non_target_missing_disk`

The remove-missing PostMaintenance path is already exercised transitively by
the existing `cmd_recover_remove_missing_pool_mutation_preserves_non_target_missing_disk`
(PoolMutation handler at `cli/src/recover.rs:2778-2801` advances to
`PostRemoveMissingMaintenance` and runs `execute_remove_missing_post_maintenance_recovery`
in the same call), so no second test is needed for parity with the
`null_underlying` coverage matrix.

## Test design

### Mirror target

The new test is a direct port of
`cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk`
(`cli/src/recover.rs:10622-10750`). The structural changes are:

| Aspect                              | null_underlying (existing)                               | missing_devids (new)                          |
|-------------------------------------|----------------------------------------------------------|-----------------------------------------------|
| Where disk3 appears in btrfs show   | `devid 3 ... path /dev/mapper/braid-disk3`               | `devid 3 size 0 used 0 path MISSING`          |
| `cryptsetup status` mock for disk3  | `cryptsetup_status_active("braid-disk3", "(null)")`      | none -- disk3 is not a live mapper            |
| `PoolState` slot                    | `null_underlying = [{mapper: braid-disk3, devid: 3}]`    | `missing_devids = [3]`                        |
| Resolver entries                    | no `/dev/vdc` entry needed                               | no `/dev/vdc` entry needed                    |

All other elements stay identical: same `Replace` journal in
`PostReplaceMaintenance` phase, same `ReplaceJournalSource::Missing`,
`restore_raid1_after_commit: false`, same pre/target memberships, same disk1
and disk-new mocks, same `BtrfsFilesystemResize { devid: 2, ... }`
expectation, same final assertions.

### Preamble (per `AGENTS.md` test convention)

```rust
// Intent: cmd_recover preserves a non-target MISSING-devid disk during
// Replace::PostReplaceMaintenance while still replaying the resize for the
// live replacement disk.
// Why it exists: the helper re-insert loop is already unit-pinned by
// recover_membership_matching_expected_reinserts_missing_devid_member; this
// test pins the composed command-boundary path -- post-maintenance
// dispatcher, live_pool_matches_membership gate, helper call site, and
// pool.json write -- against a btrfs MISSING devid. It mirrors the
// null-underlying analog cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk.
// Scenario: replace committed old -> disk-new, then unrelated disk3 went
// MISSING (flapping disk) before recovery rebuilt pool.json.
```

### Skeleton

```rust
#[test]
fn cmd_recover_replace_post_maintenance_preserves_non_target_missing_disk() {
    let f = PoolFixture::empty();
    let fs = MockFs::new(&[]);
    let new_uuid = uuid_for_name("disk-new");
    let new_uuid_text = new_uuid.to_string();

    let pre = membership_from(vec![
        membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, Some(1)),
        membership_entry("old",   "/dev/disk/by-id/virtio-old",   None, None),
        membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, Some(3)),
    ]);
    let target = membership_from(vec![
        membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, Some(1)),
        (
            new_uuid.clone(),
            disk_member_named("disk-new", "/dev/disk/by-id/virtio-disk-new", None, Some(2)),
        ),
        membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, Some(3)),
    ]);

    let journal = journal::Journal {
        started_at: "2026-01-01T00:00:00Z".into(),
        op: OpKind::Replace {
            phase: journal::ReplacePhase::PostReplaceMaintenance,
            old_uuid: uuid_for_name("old"),
            old_name: disk_name("old"),
            new_uuid: new_uuid.clone(),
            new_name: disk_name("disk-new"),
            new_target: journal::ReplaceJournalTarget {
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk-new").unwrap(),
                mode: journal::ReplaceJournalMode::ExistingLuks { enroll_key_file: None },
            },
            source: journal::ReplaceJournalSource::Missing { old_devid: 2 },
            restore_raid1_after_commit: false,
        },
        pre_membership: pre,
        target_membership: target,
    };
    journal::write_journal(&f.paths, &journal).unwrap();

    let show = ok_raw(
        "btrfs filesystem show /mnt/storage",
        "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \tTotal devices 3 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
         \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk-new\n\
         \tdevid    3 size 0 used 0 path MISSING\n",
    );
    let (mp_req, mp_out) = mountpoint_ok();
    let runner = MockRunner::default()
        .with_output(mp_req, mp_out)
        .with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint("/mnt/storage".into()),
            },
            show,
        )
        .with_output(
            CmdRequest::CryptsetupStatus { mapper: MapperName("braid-disk1".into()) },
            cryptsetup_status_active("braid-disk1", "/dev/vda"),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid { device: "/dev/vda".into() },
            cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
        )
        .with_output(
            CmdRequest::CryptsetupStatus { mapper: MapperName("braid-disk-new".into()) },
            cryptsetup_status_active("braid-disk-new", "/dev/vdb"),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid { device: "/dev/vdb".into() },
            cryptsetup_uuid_ok("/dev/vdb", &new_uuid_text),
        )
        .with_output(
            CmdRequest::BtrfsFilesystemResize {
                devid: 2,
                mount_point: MountPoint("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs filesystem resize"),
        );

    let resolver = resolver_for(&[
        ("/dev/vda", "virtio-disk1"),
        ("/dev/vdb", "virtio-disk-new"),
    ]);
    let params = f.recover_params().passphrase_file(None).build();

    let result = cmd_recover(&runner, &fs, &resolver, &params);
    result.expect("recover should preserve MISSING disk3 and resize disk-new");

    let recovered = membership::load_membership(&f.paths).unwrap();
    assert!(recovered.by_name(&disk_name("disk1")).is_some());
    assert!(recovered.by_name(&disk_name("disk-new")).is_some());
    assert!(recovered.by_name(&disk_name("old")).is_none());
    assert!(
        recovered.by_name(&disk_name("disk3")).is_some(),
        "non-target MISSING disk3 must be preserved after replace commits"
    );
    let requests = runner.requests();
    assert!(
        requests.iter().any(|request| matches!(
            request,
            CmdRequest::BtrfsFilesystemResize { devid: 2, .. }
        )),
        "post-replace recovery must resize disk-new's live devid: {requests:?}"
    );
    assert!(!f.paths.pending_op_json().exists());
}
```

### Where to insert

Place the new test immediately after
`cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk`
(after `cli/src/recover.rs:10750`) so the two symmetric tests sit together
and any future reader sees both arms of the same loop covered side-by-side.

## Reused helpers (no new code outside the test)

- `PoolFixture::empty`, `MockFs::new`, `MockRunner::default`,
  `MockRunner::with_output`
- `uuid_for_name`, `disk_name`, `disk_member_named`,
  `membership_entry`, `membership_from`
- `mountpoint_ok`, `ok_raw`, `ok_raw_empty`,
  `cryptsetup_status_active`, `cryptsetup_uuid_ok`
- `resolver_for`, `ByIdPath::parse`
- `journal::write_journal`, `membership::load_membership`
- `cmd_recover`

All exist in the test module today.

## Critical files

- `cli/src/recover.rs` -- add the new `#[test]` function inside the existing
  `mod tests` block.

No source changes. No production code touched. No new helpers needed.

## Verification

Note: the helper unit test already proves the re-insert loop works for
`pool.missing_devids`. The verification below is about proving the
composed command-boundary path (dispatcher -> gate -> helper -> write)
is wired up for `Replace::PostReplaceMaintenance` -- which is the only
coverage gap this plan closes.

1. Run the new test alone first to confirm it passes against the current
   fixed code:
   ```
   cargo test --manifest-path cli/Cargo.toml --lib \
     recover::tests::cmd_recover_replace_post_maintenance_preserves_non_target_missing_disk
   ```
   (The `just test-rust` recipe takes no args, so the targeted form must
   go through `cargo` directly. The recipe's `--lib --bin braid --test
   golden_nixos_25_11 --test tty_guard` flags are full-suite-only.)
2. Mutation check: temporarily delete the post-maintenance helper call
   site in `execute_replace_post_maintenance_recovery`
   (`cli/src/recover.rs:3291-3296`) and replace it with a passthrough
   that copies `prior` into `recovered`. Re-run the new test and confirm
   the `disk3 must be preserved` assertion fires. (The unit test would
   stay green here -- the helper itself is untouched -- so this mutation
   demonstrates the new test's distinct value at the composed boundary.)
   Restore the code afterward.
3. Run the full Rust suite as the final gate:
   ```
   just test-rust
   ```
4. No fixture refresh and no VM test run is needed -- the test is pure
   Rust with mocks and touches no parser-critical tool output.
