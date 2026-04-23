# plan-a-fix-low-resilient-moonbeam

## Context

`btrfs device scan --forget` with no device argument is kernel-global: it
unregisters every stale btrfs scan entry on the host, not just entries
for the pool braid is operating on. braid issues the bare form in three
spots:

- `cli/src/lock.rs:201` -- runtime, after `braid lock` unmounts.
- `cli/src/lock.rs:86` -- dry-run step compiler (`compile_lock_steps`).
- `cli/src/recover.rs:635` -- the `relock_and_remount` cycle.

On a single-pool NixOS host (braid's target deployment) this is
harmless: the kernel transparently rescans on next access. But the
behavior is broader than the code comment at `lock.rs:199-200` implies,
and any future host that briefly touches a non-braid btrfs filesystem
(e.g. a USB backup pool) would see its scan cache invalidated too. The
fix scopes each forget call to the exact set of LUKS mapper paths the
same code path is about to destroy, matching what the kernel comment
already claims we do.

References:

- `reference/btrfs-progs/cmds/device.c` -- userspace side.
  - No-arg `--forget` -> `btrfs_forget_devices(NULL)` -> kernel forgets all.
  - Per-path `--forget <dev>` -> `btrfs_forget_devices(path)` after
    `path_is_block_device(path)` check -> scoped forget.
- `reference/linux/fs/btrfs/volumes.c:523` (`btrfs_free_stale_devices`)
  -- the in-kernel forget path matches by devt, not by fsid. Forgetting
  only one set of devices leaves unrelated scan entries untouched, so
  the forget set MUST cover every mapper whose dm device is about to be
  destroyed (both membership and orphan mappers in lock; membership
  mappers in recover).

## Design pivot: close-set-scoped, not membership-scoped

`braid lock` already closes two sets of mappers in sequence
(`cli/src/lock.rs:220-286`): membership mappers, then orphan `braid-*`
mappers from prior crashes (supported by the journal model in
`docs/principles.md:18`). Since the kernel's forget is per-device, the
forget set must match the union of both. Otherwise a crash-created
orphan mapper can be `cryptsetup close`d with a stale btrfs scan entry
still referencing it, regressing the very race that
`BtrfsDeviceScanForget` exists to prevent (see
`tests/repro/cryptsetup-close-btrfs-held.py`).

Rule: **the forget set equals the close set.** Runtime and dry-run
compute it the same way; both paths reuse the same helper.

## Change

### `cli/src/cmd.rs`

- Line 89: change the unit variant to carry an explicit device list:
  ```rust
  BtrfsDeviceScanForget { devices: Vec<String> },
  ```
- Lines 485-488: update `to_args()` to append `devices` after
  `--forget`:
  ```rust
  CmdRequest::BtrfsDeviceScanForget { devices } => {
      let mut args = vec!["device".into(), "scan".into(), "--forget".into()];
      args.extend(devices.iter().cloned());
      CmdArgs { program: "btrfs".to_owned(), args }
  }
  ```

### `cli/src/lock.rs`

Add a helper that computes the close-set mapper-path list -- the same
`/dev/mapper/<name>` strings the existing close loops already touch --
as the union of membership-present mappers and orphan-present mappers:

```rust
/// Pool-scoped forget target: every LUKS mapper path `cmd_lock` is
/// about to destroy (membership + orphan). Filtered through fs.exists
/// because `btrfs device scan --forget <path>` rejects non-block-device
/// arguments and aborts on the first failing path.
fn lock_forget_devices<F: Filesystem + ?Sized>(
    fs: &F,
    membership: &PoolMembership,
    orphan_mappers: &[String],
) -> Vec<String> {
    let mut devs: Vec<String> = membership
        .disks
        .keys()
        .map(|name| format!("/dev/mapper/{}", mapper_name(name).0))
        .filter(|p| fs.exists(p))
        .collect();
    for entry in orphan_mappers {
        let p = format!("/dev/mapper/{entry}");
        if fs.exists(&p) {
            devs.push(p);
        }
    }
    devs
}
```

Then:

- `compile_lock_steps` (line 67): the dry-run branch already computes
  both `open_mappers` (membership + exists) and `orphan_mappers`
  (non-membership + exists). Take their union for the forget step and
  skip the step entirely when the union is empty:
  ```rust
  if pool_was_mounted {
      // umount step ...
      let mut forget_devs: Vec<String> = open_mappers
          .iter()
          .map(|m| format!("/dev/mapper/{m}"))
          .collect();
      forget_devs.extend(
          orphan_mappers.iter().map(|m| format!("/dev/mapper/{m}")),
      );
      if !forget_devs.is_empty() {
          steps.push(Step {
              risk: "safe",
              description: "btrfs device scan --forget".into(),
              commands: vec![CmdRequest::BtrfsDeviceScanForget {
                  devices: forget_devs,
              }],
          });
      }
  }
  ```

- `cmd_lock` (line 178 onward): hoist the orphan detection currently
  inline at lines 249-286 so it runs BEFORE forget. Collect the orphan
  names into a local `Vec<String>` (same shape the dry-run branch
  builds). Then call the new helper to compute the forget target, and
  only issue the command when non-empty:
  ```rust
  // After the successful-umount branch, before forget:
  let orphan_mappers = scan_orphan_mappers(fs, membership); // hoisted helper
  let forget_devs = lock_forget_devices(fs, membership, &orphan_mappers);
  if !forget_devs.is_empty() {
      let forget_result = runner.run(&CmdRequest::BtrfsDeviceScanForget {
          devices: forget_devs,
      });
      // existing match arms unchanged -- non-zero exit still downgrades
      // to a warning and continues
  }
  ```
  Reuse `orphan_mappers` in the orphan-close loop at line 249 (iterate
  the precomputed list instead of `fs.list_dir` a second time). That
  unifies the source of truth for "what are the orphans" between the
  forget call and the close loop, which is the invariant the High
  finding pins.

- `scan_orphan_mappers` is a thin wrapper around the existing
  `fs.list_dir("/dev/mapper")` + `name_from_mapper` filter at
  lines 149-162. Extract once, call twice (dry-run and runtime).

### `cli/src/recover.rs`

`relock_and_remount` closes only membership mappers (line 648). No
orphan handling. Forget target = membership-filtered-by-exists:

```rust
let forget_devs: Vec<String> = membership
    .disks
    .keys()
    .map(|name| format!("/dev/mapper/{}", config::mapper_name(name).0))
    .filter(|p| fs.exists(p))
    .collect();
if !forget_devs.is_empty() {
    let forget = runner.run(&CmdRequest::BtrfsDeviceScanForget {
        devices: forget_devs,
    }).map_err(...)?;
    // existing non-zero-exit error path unchanged
}
```

### Tests

#### argv regression (pins `to_args()` itself)

New unit test in `cli/src/cmd.rs`, alongside the existing
`btrfs_balance_raid1_soft_generates_correct_argv` pattern at
line 1234:

```rust
#[test]
// Intent: BtrfsDeviceScanForget emits `btrfs device scan --forget
// <dev>...`, never the no-arg form that forgets every scanned btrfs
// device on the host.
// Why: the no-arg form is kernel-global (volumes.c:btrfs_free_stale_devices
// with devt=0). Pool-scoped forget MUST pass explicit device paths. A
// regression to the bare form would not be caught by typed-request
// inspection in lock.rs, so pin it here at the argv layer.
// Scenario: lock builds [/dev/mapper/braid-aaa, /dev/mapper/braid-bbb];
// to_argv() appends both after --forget.
fn btrfs_device_scan_forget_generates_scoped_argv() {
    let cmd = CmdRequest::BtrfsDeviceScanForget {
        devices: vec![
            "/dev/mapper/braid-aaa".into(),
            "/dev/mapper/braid-bbb".into(),
        ],
    }
    .to_argv();
    assert_eq!(cmd.program, "btrfs");
    assert_eq!(
        cmd.args,
        vec![
            "device",
            "scan",
            "--forget",
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
        ]
    );
}
```

This is the test that directly fails if anyone re-collapses the variant
back to the bare form -- the lock-layer typed-request tests would still
pass against an accidental `to_args()` that dropped `devices`, so this
argv-layer assertion is mandatory.

#### Runtime regression (High finding)

New test in `lock.rs`:

```rust
// Intent: `braid lock` forgets the full close set (membership + orphan),
// not just membership, and passes device paths explicitly.
// Why: the kernel forget path is per-device (volumes.c:523) -- membership-
// only forget leaves orphan mappers with stale scan entries, reviving the
// cryptsetup-close-btrfs-held race for orphan mappers.
// Scenario: 2-disk pool + 1 orphan mapper (braid-ccc) present; lock runs;
// the recorded BtrfsDeviceScanForget carries all three device paths.
#[test]
fn lock_forget_includes_orphan_mappers() { ... }
```

Extend `RecordingRunner` (lines 316-349) to also capture
`BtrfsDeviceScanForget` requests. Assert the typed variant's `devices`
field equals the expected close set (not a substring on the
stringified command) -- per
`feedback_assert_typed_error_shape_not_substrings`.

Complement with a pool-scoped baseline:

```rust
// Intent: forget is scoped to the pool's own mappers, never the kernel-
// global no-arg form.
// Why: prevents `braid lock` from invalidating scan entries for an
// unrelated btrfs filesystem on the same host.
// Scenario: 2-disk pool, no orphans; recorded forget request carries
// exactly [/dev/mapper/braid-aaa, /dev/mapper/braid-bbb].
#[test]
fn lock_forget_is_pool_scoped() { ... }
```

#### Dry-run regression (Low finding)

Replace the loose substring assertion in
`dry_run_render_lock_mounted_2_disks` (lines 1366-1371) with explicit
checks on the forget step's device list, and add a new case that
verifies the step is omitted when there are no mappers to close:

```rust
#[test]
fn dry_run_lock_forget_step_lists_scoped_devices() {
    // pool_was_mounted=true, open_mappers=[braid-aaa, braid-bbb], orphans=[];
    // assert the forget step's CmdRequest carries
    //   devices = [/dev/mapper/braid-aaa, /dev/mapper/braid-bbb]
    // by inspecting the compiled Vec<Step>, not rendered text.
}

#[test]
fn dry_run_lock_forget_step_includes_orphans() {
    // open_mappers=[braid-aaa], orphans=[braid-orphan];
    // assert devices = [/dev/mapper/braid-aaa, /dev/mapper/braid-orphan].
}

#[test]
fn dry_run_lock_forget_step_omitted_when_no_mappers() {
    // pool_was_mounted=true, open_mappers=[], orphans=[];
    // assert no Step with CmdRequest::BtrfsDeviceScanForget is emitted.
    // (The umount step is still emitted.)
}
```

These bind to the typed `Vec<Step>` / `CmdRequest` structure, so they
fail closed if `compile_lock_steps` regresses to the no-arg form or
drops the orphan union.

#### Recover-side update

Update `tests/repro/btrfs-replace-interrupted-mid-flight` and the
recover unit tests only to the extent their `MockRunner.with_output`
mocks need the new variant shape. No new recover tests -- the
membership-only rule is load-bearing by construction (no orphan scan
there).

### Non-goals

- No migration path; braid has no backwards-compat obligation
  (`AGENTS.md`).
- No change to error semantics: forget failure in `cmd_lock` still
  downgrades to a warning and continues (lines 202-214); forget failure
  in `relock_and_remount` still returns `RecoverError::Failed`
  (lines 637-643).
- No VM test for the pool-scoping delta. It only manifests when a
  second, non-braid btrfs filesystem coexists on the same host, which
  is outside the current test fleet's fixture model and disproportionate
  to a Low finding.

## Verification

1. `just test-rust` -- runs `cargo test --lib --test
   golden_nixos_25_11`. The updated mocks, the new
   `lock_forget_includes_orphan_mappers` / `lock_forget_is_pool_scoped`
   tests, and the tightened dry-run assertions must all pass.
2. `just test-vm braid-lock-btrfs-held` -- end-to-end lock/unlock on a
   live kernel (3-disk pool, 3 cycles). Pins that scoped forget still
   clears the same registry entries the bare form cleared.
3. `just test-repro repro-cryptsetup-close-btrfs-held` -- the
   repro that documents why forget is load-bearing; must still reach
   the expected outcome under the new variant.
4. `just test-repro repro-btrfs-replace-interrupted-mid-flight` --
   covers the `relock_and_remount` forget callsite end-to-end.
5. Manual grep: `rg 'BtrfsDeviceScanForget\b' cli/src` returns only the
   parameterized form -- no bare variants remain.

## Critical files

- `cli/src/cmd.rs` -- enum + `to_args`.
- `cli/src/lock.rs` -- step compiler, `cmd_lock`, orphan-scan
  extraction, tests.
- `cli/src/recover.rs` -- `relock_and_remount`, tests.
- `reference/btrfs-progs/cmds/device.c`,
  `reference/linux/fs/btrfs/volumes.c` -- read-only references.
