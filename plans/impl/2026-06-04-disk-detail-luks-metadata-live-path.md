# Fix: disk-detail LUKS metadata must read from the live backing path

## Context

Commit `8593c9fe` ("query present hardware through live paths") established
decision 024's invariant that **present-device probes use the live backing path,
not the persisted by-id handle**, because a by-id handle can drift while the LUKS
UUID still proves the member is present. That commit routed the TUI's SMART/temperature
probe and status' model/serial probe through the live `underlying` path -- but it
left one present-device probe behind: the disk-detail **LUKS metadata** read.

`build_disk_luks_states` (`cli/src/tui/probe.rs#build_disk_luks_states`) still calls
`probe_disk_luks_metadata(runner, by_id_path)` -- always the persisted by-id path.
For a mounted, identity-verified member whose by-id handle has drifted, `cryptsetup
luksDump` then runs against a stale path and fails, so the Disk Detail popup renders
the literal "LUKS metadata unavailable" (`cli/src/tui/view/mod.rs`, the `else` arm of
the `if let Some(info) = luks` block) for an open, healthy disk. The live path is
already in hand: `mounted_classification` carries `Some(device.underlying)` for present
members and `build_disk_luks_states` already destructures it into `underlying_present`.

Outcome: the metadata read joins SMART/model under the same live-path invariant --
live backing path for verified-present (`Unlocked`) members, by-id otherwise -- so the
popup keeps showing cipher / key size / keyslots through by-id drift.

### Why the live path is a valid `luksDump` target (not an assumption)

`underlying` is the `device:` field from `cryptsetup status` -- the LUKS *container*
(e.g. `/dev/vda`), not the decrypted mapper. `probe_pool` already runs `cryptsetup
luksUUID` against that exact `underlying` (`cli/src/probe.rs`, the pool-device loop that
populates `PoolDevice.underlying`). `luksUUID` and `luksDump` both read the on-disk LUKS
header, so routing `luksDump` to `underlying` reads the same header braid already reads.

## The fix (implementation)

One file: `cli/src/tui/probe.rs`.

1. **Route the metadata read through the live path only for ownership-verified
   (`Unlocked`) members, with a by-id fallback.** In `build_disk_luks_states`, `lock`
   (a `Copy` enum: `#[derive(Clone, Copy, Debug, PartialEq, Eq)]`) and
   `underlying_present` are moved into the `DiskLuksState` struct, so compute `metadata`
   just before the struct literal, gating the live path on `lock`:

   ```rust
   let metadata_device = match underlying_present.as_deref() {
       Some(underlying) if lock == DiskLockState::Unlocked => underlying,
       _ => by_id_path.as_str(),
   };
   let metadata = probe_disk_luks_metadata(runner, metadata_device);
   disk_luks_states.insert(
       disk_name.clone(),
       DiskLuksState { lock, underlying_present, metadata },
   );
   ```

   **Why the `Unlocked` gate is load-bearing (not the unconditional `unwrap_or` an
   earlier draft used).** `underlying_present` is `Some` in the verified case *and* in
   unverified ones: `fallback_disk_luks_lock` returns `(Unknown, Some(underlying))` for
   backing-path mismatch, UUID-probe failure, and UUID mismatch
   (`cli/src/tui/probe.rs#fallback_disk_luks_lock`). In the backing-path-mismatch case
   `underlying` is a *foreign* device that merely holds braid's mapper name -- different
   from the disk's declared by-id handle -- so an unconditional `unwrap_or` would dump
   that foreign mapper's cipher/keyslots under the declared disk's identity. Gating on
   `Unlocked` admits exactly the two verified sources -- the mounted UUID-matched path
   (`mounted_classification` only ever inserts `Unlocked`) and the fallback's
   `found_uuid == expected_uuid` arm -- and routes every `Locked`/`Unknown` member to
   the by-id handle, preserving today's behavior for those cases.

   This uses the `underlying_present` already computed in the function rather than
   re-reading `mounted_classification` -- one expression covers both `Unlocked` sources
   and is simpler than mirroring the SMART loop. The drift protection itself comes from
   the **mounted** path: `mounted_classification` matches members by LUKS UUID, never
   by-id, so a mounted member's by-id may be arbitrarily drifted while `underlying` stays
   live. The fallback `(Unlocked, Some(underlying))` arm is reached only *after* by-id has
   canonicalized to the same device as `underlying` (the path check passed) and the UUID
   matched -- so by-id is **not** drifted there; routing it through `underlying` is for
   uniformity and to skip a redundant resolve, not recovery (reading by-id would succeed
   too). Every `Locked` or `Unknown` member -- and any `Unlocked` member with no observed
   backing -- falls back to by-id.

2. **Rename the `probe_disk_luks_metadata` parameter `by_id_path` -> `device`.** After
   the change the function receives a live path as often as a by-id path; the old name
   is now misleading and is exactly what would re-seed this finding. Update the one use
   inside the body (`device: device.to_owned()`).

## Test changes (`cli/src/tui/probe.rs`, `#[cfg(test)] mod tests`)

The contract has two halves: (1) cipher shown == the disk's real cipher, sourced from
the live device, surviving by-id drift, for a verified-present member; and (2) a foreign
/ ownership-unverified mapper must not surface the live device's metadata under the
declared disk. The three tests below pin both halves.

### A. New regression test -- mounted present member, drifted by-id

Mirror `smartctl_health_for_present_member_uses_live_underlying`. Name it
`luks_metadata_for_present_member_uses_live_underlying`.

- Build the runner from `one_disk_mounted_pool_runner()` (toshiba mounted; live
  underlying `/dev/vda`; LUKS UUID `1111...`). It already registers the pool-probe
  `CryptsetupStatus` / `CryptsetupLuksUuid` for `/dev/vda`, so the mounted path
  populates `mounted_classification[toshiba] = (Unlocked, Some("/dev/vda"))`.
- Chain two `.with_output(...)` overrides:
  - `CryptsetupLuksDump { device: "/dev/vda" }` -> `ok_raw("cryptsetup luksDump",
    &luks_dump_json("aes-xts-plain64"))` (the live path succeeds).
  - `CryptsetupLuksDump { device: "/dev/disk/by-id/braid-toshiba" }` ->
    `err_raw("cryptsetup luksDump", "drifted handle\n", 1)` (the stale by-id path
    fails -- this is the exact drift scenario; a buggy code path that reads by-id
    yields `None`).
- Call `probe_pool_for_tui` with `tui_disks_with_by_id` mapping toshiba to
  `/dev/disk/by-id/braid-toshiba` and `crate::test_fixtures::mock_virtio_backing_path_resolver()`.
  Bind `let (states, _pool) = ...unwrap();` (metadata lives in `states`, not in
  `PoolState`).
- Assert `states["toshiba"].metadata.as_ref().map(|i| i.cipher.as_str()) ==
  Some("aes-xts-plain64")`. With the bug present this is `None` ("unavailable"); with
  the fix it is the live cipher.

This also closes a real coverage gap: no existing test asserts metadata on the
*mounted* present-member path.

### B. Update existing test `probe_classifies_unmounted_open_and_closed_mappers`

This unmounted/fallback test currently registers toshiba's `luksDump` mock at the
by-id path and its preamble states metadata is "routed through the configured device
path, not the live backing path" -- both encode the old behavior.

- toshiba is fallback-`Unlocked` with `underlying_present = Some("/dev/vdb")`, so move
  its `CryptsetupLuksDump` mock device from `/dev/disk/by-id/braid-toshiba` to
  `/dev/vdb`. (Leave its existing `CryptsetupStatus`/`CryptsetupLuksUuid` for `/dev/vdb`
  and the `.with_path(".../braid-toshiba", "/dev/vdb")` resolver override untouched.)
- ironwolf is `Locked` with `underlying_present = None`, so its `luksDump` mock stays at
  the by-id path -- this now exercises the by-id fallback arm. Leave it unchanged.
- Rewrite the preamble to state the corrected contract: an open mapper's metadata is
  read from the live backing path; a closed mapper falls back to the persisted by-id
  path. Keep the existing cipher assertions (`aes-xts-plain64` / `serpent-xts-plain64`)
  -- they pass once the toshiba mock moves.

### C. New test -- backing-path-mismatch mapper must not leak foreign metadata

Add `probe_fallback_backing_path_mismatch_does_not_read_foreign_metadata`, mirroring
`probe_fallback_classifies_foreign_uuid_mapper_as_unknown`'s shape. This is the branch
where the gate matters *behaviorally*: by-id and `underlying` resolve to **different**
devices, so the observed live path is a genuinely foreign mapper, not the declared disk.
(The UUID-mismatch branch is a weaker target -- it is reached only after the path check
passes, so by-id and `underlying` are already the same device there, and a test on it
would pin only mock-string routing rather than a real foreign-device read.)

- `disk_by_id`: toshiba -> `/dev/disk/by-id/braid-toshiba`.
- Runner:
  - `CryptsetupStatus { mapper: braid-toshiba }` -> active with `device: /dev/vdz` (the
    open mapper is backed by a foreign `/dev/vdz`).
  - `CryptsetupLuksDump { device: "/dev/vdz" }` -> `ok_raw(..., &luks_dump_json("foreign-cipher"))`
    -- the foreign device has readable metadata (the bait an ungated read would take).
  - Register nothing for the by-id `luksDump` and no `luksUUID` (the path check returns
    `Unknown` before any UUID probe).
- Resolver: `MockBackingPathResolver::default().with_path("/dev/disk/by-id/braid-toshiba",
  "/dev/vdb")`. So `canonicalize(by-id) = /dev/vdb` while `canonicalize("/dev/vdz") =
  /dev/vdz` (no override): the path check `expected_path != found_path` fails, so
  `fallback_disk_luks_lock` returns `(Unknown, Some("/dev/vdz"))`
  (`cli/src/tui/probe.rs#fallback_disk_luks_lock`, the path-mismatch return).
- Call `probe_pool_for_tui` unmounted (`StubFs::unmounted_with_paths(&[])`), bind
  `let (states, _pool) = ...unwrap();`.
- Assert all three:
  - `states["toshiba"].lock == DiskLockState::Unknown`
  - `states["toshiba"].underlying_present.as_deref() == Some("/dev/vdz")` -- the foreign
    backing *was* observed, so an ungated read would have used it.
  - `states["toshiba"].metadata == None` -- the gate routed the read to the by-id request
    string, which is unregistered -> `CmdError::MissingMock` -> `.ok()?` -> `None`.

With the gate, `/dev/vdz`'s `"foreign-cipher"` is never requested. Drop the gate and the
read targets `/dev/vdz`, yielding `Some("foreign-cipher")` under toshiba -- the leak this
test guards. Because by-id (`/dev/vdb`) and the backing (`/dev/vdz`) are different devices
here, the `metadata == None` assertion reflects a real behavioral guarantee, not a
mock-string artifact. (`probe_fallback_classifies_foreign_uuid_mapper_as_unknown` stays
as-is; it already pins the `Unknown` lock classification for the UUID-mismatch branch.)

(`probe_status_active_metadata_failed_decouples_lock_and_metadata` needs no change: it is
an `Unlocked` member whose by-id `luksDump` mock returns an error. Post-fix it reads the
live `/dev/vdb` path, which is unregistered -> `CmdError::MissingMock` -> `.ok()?` ->
`None`; it still asserts `metadata == None`. Optional polish: register the `/dev/vdb`
error mock so the failure is modeled on the path actually queried; not required for
correctness.)

## Doc sync

`docs/design/decisions/024-luks-uuid-identity.md` needs two edits:

1. **Broaden the live-path bullet.** It is currently titled "Present-device hardware
   probes use live paths" and enumerates only "lsblk model/serial and smartctl" -- but
   `cryptsetup luksDump` metadata is not a hardware probe. Retitle to "**Present-device
   probes use live paths.**" and extend the body to include the metadata dump and its
   ownership gate:

   > Queries such as lsblk model/serial and smartctl use the live backing path
   > (`PoolState::underlying_for_uuid`), and the TUI disk-detail LUKS metadata dump
   > (`cryptsetup luksDump`) reads the live backing path for a verified-present
   > (`Unlocked`) member -- not persisted by-id setup/repair handles that can drift
   > while the disk is still present. Metadata for locked or ownership-unverified
   > mappers stays on the by-id handle.

2. **Add to "Tests That Enforce This."** That section lists several `tui/probe.rs`
   tests but none for the metadata read; append a bullet covering the new TUI tests:

   > - `cli/src/tui/probe.rs` unit tests pin that the disk-detail LUKS metadata dump
   >   reads the live backing path for a verified-present member (surviving by-id
   >   drift), and that a foreign / ownership-unverified mapper does not surface the
   >   live device's metadata under the declared disk.

This keeps the ADR honest per AGENTS.md ("Architecture Authority") and stops a future
reader from assuming metadata is exempt from the invariant.

## Out of scope (considered, rejected)

- **Unifying the TUI live-path resolution into `PoolState::underlying_for_uuid`** (used
  by `doctor.rs`/`replace.rs`). `build_disk_luks_states` runs *before* `PoolState` is
  assembled and also in the unmounted branch where no `PoolState` exists -- which is
  precisely why it owns `mounted_classification` + `fallback_disk_luks_lock`. Folding it
  into `underlying_for_uuid` is not possible here.
- **Folding the separate SMART loop into this pass.** The SMART/usage pass runs only in
  the mounted branch, after df/usage, and re-iterates `disks.by_id`. Merging it tangles
  two phases for negligible gain. Keep the fix local.

## Verification

- `just test-rust` -- runs the CLI unit tests (package `braid-cli`). Expect the new
  `luks_metadata_for_present_member_uses_live_underlying`, the new
  `probe_fallback_backing_path_mismatch_does_not_read_foreign_metadata`, and the updated
  `probe_classifies_unmounted_open_and_closed_mappers` to pass. Sanity-check that both
  halves of the change are load-bearing: temporarily drop the `Unlocked` gate and confirm
  the backing-path-mismatch test fails (metadata becomes `Some("foreign-cipher")`); revert
  the live-path routing entirely and confirm the mounted-member test fails (metadata
  `None`).
- TUI snapshot tests under `cli/src/tui/view/snapshots/` are unaffected (they assert on
  rendering given a populated `DiskLuksInfo`, not on which device path produced it); no
  `cargo insta` review expected. `just test-rust` will flag a snapshot diff if that
  assumption is wrong.
- No NixOS VM test exercises this path (it's pure model-building logic over mocked
  command output), so `just test-vm` is not required for this change.
