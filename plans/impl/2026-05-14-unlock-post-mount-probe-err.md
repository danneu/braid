# Plan: pin `cmd_unlock` tolerance of post-mount `probe_pool` Err

## Context

`cmd_unlock` enriches `pool.json` with live `devid` / `added_at`
metadata via a best-effort block at `cli/src/unlock.rs:122-130`:

```rust
if let Ok(pool_after) = probe::probe_pool(runner, fs, mount_point) {
    membership::refresh_pool_metadata(&pool_after, params.paths);
}
```

The `if let Ok(...)` makes this best-effort: any `Err` from `probe_pool`
(e.g. a `btrfs filesystem show` parser drift after a nixpkgs bump) is
silently swallowed and membership metadata stays unenriched. That
contract is named in the pinning comment but only one of its two
branches is asserted by a dedicated test today:

- `unlock_tolerates_post_mount_probe_mounted_false`
  (`cli/src/unlock.rs:955`) pins the `Ok(PoolState { mounted: false,
  devices: vec![], ... })` race -- it uses `mount_fs` (rootfs-only
  mountinfo) so `probe_pool` early-returns at
  `cli/src/probe.rs:331-340` before reaching any runner call.
- The `Err(_)` arm is incidentally exercised by tests that use
  `unlock_storage_fs` and never seed `BtrfsFilesystemShow` (e.g.
  `unlock_bricked_disk_uses_degraded_mount` at
  `cli/src/unlock.rs:242`), but no test asserts that tolerance as
  the property under test.

If a future refactor promoted that `if let Ok(...)` to `?`, narrowed
the swallowed-error set (e.g. tolerate `ProbeError::Cmd` but bubble
`ProbeError::Parse`), or routed failures through a new variant, the
realistic production trigger -- a `btrfs filesystem show` parser
drift after a nixpkgs bump emitting `ProbeError::Parse(_)` -- would
turn a healthy mount into a hard failure with no signal in CI. A
dedicated pin test that simulates that exact parser-drift error
makes the intent explicit, the failure mode obvious, and protects
the membership-data-unchanged invariant for the Err branch.

## Scope

Add one Rust unit test in `cli/src/unlock.rs::tests` and tighten the
pinning comment to reference both tests. Strictly a test-only change;
no production behavior changes.

The same `if let Ok(pool_after) = probe_pool(...)` best-effort idiom
exists at `cli/src/add.rs:1244` and `cli/src/replace.rs:806` and is
similarly unpinned. Those sites also affect persisted metadata --
they mutate an in-memory `*_membership` that is unconditionally
saved a few lines later (`cli/src/add.rs:1247`,
`cli/src/replace.rs:817`). **Out of scope** for this plan: pin
unlock first because it is the cleanest target (single call site,
existing sibling test, smallest scaffold change). `add` and
`replace` need their own follow-up pin tests with the same shape
adapted to their explicit-save call graph.

## Files

- **Edit** `cli/src/unlock.rs`:
  - Extend the test-module import at `cli/src/unlock.rs:219` from
    `use crate::cmd::{CmdRequest, MockRunner};` to
    `use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};`.
    The new test inlines a `RawCommandOutput { ... }` literal for
    the malformed `BtrfsFilesystemShow` mock, and no existing test
    in this module references the type directly.
  - Add new test `unlock_tolerates_post_mount_probe_err` directly
    below `unlock_tolerates_post_mount_probe_mounted_false`
    (currently ending at line 1045).
  - Update the comment block at `cli/src/unlock.rs:122-127` to name
    both pin tests and document the Err-arm tolerance explicitly.

No other files change. No fixtures change.

## Existing scaffolding to reuse (no new helpers)

Every primitive needed already exists; the new test is a near-mirror
of the existing tolerance test with two structural differences:

| Concern | Helper | Location |
|---|---|---|
| Filesystem whose mountinfo declares `/mnt/storage` as btrfs | `unlock_storage_fs` | `cli/src/test_fixtures/unlock.rs:24` (wraps `MockFs::storage` at `cli/src/test_fixtures/shared.rs:105`) |
| 3-disk seeded membership with `devid=None`, `added_at=None` | `unlock_three_disk_membership` (aliased to `three_disk_membership`) | `cli/src/test_fixtures/mount.rs` -- import already present in `unlock.rs::tests` |
| Per-disk LUKS UUID, passphrase, mapper-open, mount, balance-status mocks | `luks_uuid_ok`, `unlock_with_test_passphrase_ok`, `unlock_with_open_mapper_ok`, `unlock_with_mount_ok`, `unlock_btrfs_device_scan_ok`, `unlock_btrfs_balance_status_idle`, `with_luks_dump_text_luks2_for`, `with_mappers_closed` | All already imported in `unlock.rs::tests` and used by the sibling tolerance test |
| Seed-and-load pool.json baseline | `membership::save_membership` + `membership::load_membership` | `cli/src/membership.rs:425` (called the same way the sibling test does) |

`refresh_pool_metadata` (`cli/src/membership.rs:628`) only writes
`devid` and `added_at` via `enrich_from_pool_state`
(`cli/src/membership.rs:594`); a post-call assertion that both
remain `None` is sufficient to prove no enrichment occurred.

## The two structural differences from the sibling test

1. **Mountinfo body.** Use `unlock_storage_fs(...)` (declares
   `/mnt/storage` as btrfs) instead of `mount_fs(...)` (rootfs only).
   This forces the post-mount `probe_pool` to proceed past
   `fstype_at_mount_via_fs` and try the runner call.

2. **Seed `BtrfsFilesystemShow` with exit 0 + malformed stdout.**
   Simulate a real `btrfs filesystem show` parser drift: the stdout
   body lacks the `Total devices` line, so `parse_btrfs_filesystem_show`
   (`cli/src/parse/btrfs_filesystem_show.rs:84-103`) returns
   `Err(ParseError::MissingField { field: "Total devices" })`. The
   probe propagates this as `ProbeError::Parse(_)` and the
   `if let Ok(...)` arm short-circuits. The malformed body mirrors
   the parser's canonical malformed-input case at
   `cli/src/parse/btrfs_filesystem_show.rs:285-294`
   (`btrfs_show_rejects_malformed_inline`). This is what makes the
   pin diagnostic of the realistic production failure mode --
   `ProbeError::Cmd(MissingMock)` would only have caught a
   strictly-broader regression.

Everything else mirrors `unlock_tolerates_post_mount_probe_mounted_false`:
same 3-disk topology, same passphrase file, same balance-status
seeding (`emit_paused_balance_warning` runs after the best-effort
block at `cli/src/unlock.rs:135` and requires the
`BtrfsBalanceStatus` mock).

## Test body sketch

```rust
// Intent: `cmd_unlock` must tolerate a post-mount `probe_pool` that
//   returns `Err(ProbeError::Parse(_))` (a `btrfs filesystem show`
//   parser drift) without enriching membership metadata and without
//   failing.
// Why it exists: the post-mount enrichment block at unlock.rs:128 is
//   best-effort. The sibling test pins the `Ok(mounted=false)` race;
//   this pins the realistic Err branch -- a parser drift after a
//   nixpkgs bump -- so a future refactor that promotes
//   `if let Ok(...)` to `?` or narrows the swallowed-error set
//   (tolerating Cmd but not Parse) fails CI with a clear signal.
// Scenario: 3-disk pool, clean mount. Mountinfo declares /mnt/storage
//   as btrfs so probe_pool proceeds past the early-return. The
//   `BtrfsFilesystemShow` mock returns exit 0 with malformed stdout
//   lacking the `Total devices` line, so
//   `parse_btrfs_filesystem_show` returns `MissingField` and the
//   probe yields `Err(ProbeError::Parse(_))`. After cmd_unlock
//   returns Ok, pool.json's membership data must still show
//   devid=None / added_at=None for every disk (no enrichment).
#[test]
fn unlock_tolerates_post_mount_probe_err() {
    let (_state_dir, sp) = isolated_paths();
    let config = test_config();
    let membership = unlock_three_disk_membership();
    membership::save_membership(&membership, &sp)
        .expect("seed pool.json for assertion baseline");

    let fs = unlock_storage_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/disk/by-id/virtio-disk3",
    ]);

    let mp = MountPoint("/mnt/storage".to_owned());
    let (uuid1_req, uuid1_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk1",
        "11111111-1111-1111-1111-111111111111",
    );
    let (uuid2_req, uuid2_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk2",
        "22222222-2222-2222-2222-222222222222",
    );
    let (uuid3_req, uuid3_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk3",
        "33333333-3333-3333-3333-333333333333",
    );
    let (scan_req, scan_out) = unlock_btrfs_device_scan_ok();
    let (balance_req, balance_out) = unlock_btrfs_balance_status_idle(&mp);
    let runner = MockRunner::default()
        .with_output(
            CmdRequest::MountpointCheck { path: mp.clone() },
            unlock_err_raw("mountpoint", 1, ""),
        )
        .with_output(uuid1_req, uuid1_out)
        .with_output(uuid2_req, uuid2_out)
        .with_output(uuid3_req, uuid3_out)
        .with_luks_dump_text_luks2_for(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ])
        .with_mappers_closed(&["braid-disk1", "braid-disk2", "braid-disk3"]);
    let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk1");
    let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk2");
    let runner = unlock_with_test_passphrase_ok(runner, "/dev/disk/by-id/virtio-disk3");
    let runner =
        unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk1", "braid-disk1");
    let runner =
        unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk2", "braid-disk2");
    let runner =
        unlock_with_open_mapper_ok(runner, "/dev/disk/by-id/virtio-disk3", "braid-disk3");
    let runner = runner.with_output(scan_req, scan_out);
    let runner = unlock_with_mount_ok(runner, "/dev/mapper/braid-disk1", &mp)
        .with_output(balance_req, balance_out);
    // Seed BtrfsFilesystemShow with exit 0 + malformed stdout so
    // parse_btrfs_filesystem_show returns MissingField and the
    // post-mount probe yields Err(ProbeError::Parse(_)). The body
    // mirrors the parser's canonical malformed fixture at
    // cli/src/parse/btrfs_filesystem_show.rs:285-294.
    let runner = runner.with_output(
        CmdRequest::BtrfsFilesystemShow {
            mount_point: mp.clone(),
        },
        RawCommandOutput {
            cmd: "btrfs filesystem show".to_owned(),
            stdout: "This is not btrfs output at all\nrandom garbage data".to_owned(),
            stderr: String::new(),
            exit_status: 0,
        },
    );
    let tmp = unlock_passphrase_file();

    let result = cmd_unlock(
        &runner,
        &fs,
        &UnlockParams {
            config: &config,
            membership: &membership,
            paths: &sp,
            passphrase_stdin: false,
            passphrase_file: Some(tmp.path()),
            key_file: None,
            allow_degraded: false,
            dry_run: false,
        },
    );

    result.expect(
        "unlock should tolerate probe_pool returning Err(ProbeError::Parse(_))",
    );

    let loaded = membership::load_membership(&sp)
        .expect("pool.json should still be loadable after unlock");
    for name in ["disk1", "disk2", "disk3"] {
        let disk_name = crate::types::DiskName::parse(name).unwrap();
        let member = loaded
            .by_name(&disk_name)
            .map(|(_, member)| member)
            .unwrap_or_else(|| panic!("missing disk {name} in pool.json"));
        assert!(
            member.devid.is_none(),
            "{name}.devid must remain None when probe_pool returns Err, got: {:?}",
            member.devid
        );
        assert!(
            member.added_at.is_none(),
            "{name}.added_at must remain None when probe_pool returns Err, got: {:?}",
            member.added_at
        );
    }
}
```

## Comment update at `cli/src/unlock.rs:122-127`

Before:

```rust
// Enrich pool.json with live metadata (devid, added_at) -- best-effort.
// A rare race where probe_pool sees mounted=false after a successful
// mount leaves `pool_after.devices` empty, so refresh_pool_metadata
// no-ops. That is acceptable: correctness never depends on this write
// (see contract above). Pinned by
// unlock_tolerates_post_mount_probe_mounted_false.
```

After:

```rust
// Enrich pool.json with live metadata (devid, added_at) -- best-effort.
// Two outcomes are tolerated and leave membership data unenriched:
//   * `Ok(PoolState { mounted: false, devices: vec![], ... })` -- a
//     mountinfo race after a successful mount; refresh_pool_metadata
//     still runs and re-saves, but enrich_from_pool_state walks an
//     empty devices vec so no fields change.
//   * `Err(_)` from probe_pool itself (e.g. a parser drift in
//     `btrfs filesystem show`) -- the `if let Ok` arm short-circuits,
//     refresh_pool_metadata is never called, and pool.json is not
//     rewritten.
// Correctness never depends on this enrichment (see contract above).
// Pinned by unlock_tolerates_post_mount_probe_mounted_false and
// unlock_tolerates_post_mount_probe_err.
```

## Verification

Run `just test-rust` to validate the full Rust lane. For a scoped
run during development, use cargo directly -- `just test-rust` does
not forward args (`just test-rust <name>` is parsed as a second
recipe and fails):

```
cargo test --lib unlock_tolerates_post_mount
```

(matches both `_mounted_false` and `_probe_err` tests).

To prove the new test actually pins the contract (one-shot during
implementation, do not commit):

1. Temporarily change `cli/src/unlock.rs:128-130` from

   ```rust
   if let Ok(pool_after) = probe::probe_pool(runner, fs, mount_point) {
       membership::refresh_pool_metadata(&pool_after, params.paths);
   }
   ```

   to

   ```rust
   let pool_after =
       probe::probe_pool(runner, fs, mount_point).map_err(MountError::from)?;
   membership::refresh_pool_metadata(&pool_after, params.paths);
   ```

   The `.map_err(MountError::from)?` chain is required because
   `UnlockError` only has `From<MountError>` (`cli/src/unlock.rs:16`),
   not `From<ProbeError>`; `MountError` carries the `Probe(#[from]
   ProbeError)` bridge (`cli/src/mount.rs:19`). A bare `?` will not
   compile.
2. Run `cargo test --lib unlock_tolerates_post_mount_probe_err` --
   it must fail. The surfaced error chain is
   `UnlockError::Mount(MountError::Probe(ProbeError::Parse(MissingField)))`.
3. Run `cargo test --lib unlock_tolerates_post_mount_probe_mounted_false`
   -- it must still pass (its branch returns `Ok(mounted=false)`,
   not `Err`).
4. Revert step 1.

No VM tests, fixtures, or parser-compatibility lanes are affected.

## Out of scope

- No change to production behavior.
- No new fixtures or test helpers.
- No expansion to `cli/src/add.rs:1244` or `cli/src/replace.rs:806`
  (sibling best-effort sites). They share the same shape and also
  affect persisted metadata via a downstream `save_membership` call,
  but pinning all three at once would inflate this change beyond a
  single test. Track as separate follow-up pins -- each will need a
  pool.json-after-Err assertion adapted to that command's explicit
  save semantics.
