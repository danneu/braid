# Plan: pivot per-disk LUKS state out of `PoolState`

## Context

`cli/src/tui/view/mod.rs:1097-1102` derives a disk's `Status: locked/unlocked` label from `pool.current().disk_usage.contains_key(name)`. `pool.current()` returns `Some(&PoolState)` only for `PoolStatus::{Mounted, Refreshing, ErrorStale}`. For every other status -- `Loading`, `NotMounted`, `Error` -- every disk renders as `Status: locked`, regardless of what mappers are actually open. The same view path's cipher/keyslot block reads from `PoolState.luks_info`, which is only populated when the pool is mounted (`cli/src/tui/probe.rs:35-37` early-returns `Ok(None)` on `!domain.mounted`). The popup says `LUKS metadata unavailable` in exactly the diagnostic state the operator opens it for -- failed mount, partial unlock, post-crash.

The underlying probes (`cryptsetup status braid-<name>`, `cryptsetup luksDump <by_id>`) work regardless of pool mount. The bug is architectural: per-disk LUKS state is conflated with pool state in `PoolState`, and discarded whenever the pool isn't mounted. The fix is to lift per-disk LUKS state onto `Model` as a sibling of `pool`, populated every probe regardless of mount status. This mirrors the existing `session_temperature_stats` pattern (`cli/src/tui/model.rs:263`, updated by reducer at `cli/src/tui/app.rs:148-162`) and makes `view_disk_detail` honest across every pool state.

The `null_underlying` claim in the originating finding is partly off: when the pool is mounted, devid persistence binds null-underlying members back into `disk_usage`, so the popup renders `unlocked` today -- technically correct (the mapper is open) but failing to surface that the backing device is gone. We address that as a separate axis (`underlying_present`), not by overloading the lock-state enum.

## Approach

Two binary axes per disk, populated each probe regardless of mount:

- `lock`: `Unlocked | Locked | Unknown` (sourced from `cryptsetup status`; `Unknown` covers probe failure).
- `underlying_present`: `Option<String>` (Some(path) when `cryptsetup status` reports a real underlying device; None for `(null)` or when the mapper is closed).

Plus `metadata: Option<DiskLuksInfo>` (cipher/keyslots/keyslot count) from `cryptsetup luksDump <by_id>`, independent of lock state.

`view_disk_detail` reads from this new map. Pool-membership concerns (Allocations table, btrfs device errors) keep gating on `pool.current()` -- those legitimately require a mounted pool.

The change lands as two commits to keep review surface small:

1. **Additive**: introduce `disk_luks_states` on `Model`, populate it in the TUI probe, switch `view_disk_detail` to read from it.
2. **Cleanup**: delete the now-redundant `luks_info` field from `PoolState` and any probe-side construction.

## Files to change

### `cli/src/tui/model.rs`

Add new types adjacent to `DiskLuksInfo`:

```rust
/// Per-declared-disk lock state surfaced to the TUI independently of
/// pool mount status, so `view_disk_detail` can render the truth in
/// `NotMounted`/`Error` states where `PoolStatus::current()` is `None`.
/// `Unknown` covers probe failure AND mapper-basename collisions that
/// fail the ownership check -- the TUI must never imply a disk is open
/// without identity confirmation.
pub enum DiskLockState {
    Unlocked,
    Locked,
    Unknown,
}

/// Mount-independent per-disk LUKS snapshot for TUI diagnostics. Lives
/// on `Model` as a sibling of `pool` (not inside `PoolState`) so it
/// survives every `PoolStatus` transition.
pub struct DiskLuksState {
    pub lock: DiskLockState,
    /// Backing block device reported by `cryptsetup status`. None when
    /// the mapper is closed OR when an open mapper reports `(null)`
    /// (hot-unplugged backing device). Kept distinct from `lock` because
    /// physical presence is orthogonal to cryptographic lock state.
    pub underlying_present: Option<String>,
    pub metadata: Option<DiskLuksInfo>,
}
```

Add field to `Model` (sibling of `pool`):

```rust
pub disk_luks_states: HashMap<String, DiskLuksState>,
```

Initialize empty in `Model::new` and `Model::new_demo` (`cli/src/tui/model.rs:320-373`).

### `cli/src/tui/probe.rs`

Change `probe_pool_for_tui` return type from
`Result<Option<PoolState>, String>` to
`Result<(HashMap<String, DiskLuksState>, Option<PoolState>), String>`
(tuple, not new named struct -- less invasive, no new public type).

Restructure the function so the disk-level loop runs BEFORE the `!domain.mounted` branch:

1. Run `probe_pool` -> `domain`. (Already done at `cli/src/tui/probe.rs:33`.)
2. Build two identity-keyed name maps from the existing per-disk inputs (mirror the pattern at `cli/src/tui/probe.rs:59-66`):
   - `uuid_to_name: HashMap<&LuksUuid, &str>` from `disk_luks_uuid`.
   - `persisted_devid_to_name: HashMap<u64, &str>` from `disk_devid`.
3. Pre-populate a `mounted_classification: HashMap<String, (DiskLockState, Option<String>)>` from the mounted-pool probe output, joining on identity (never on mapper basename, never on disk name):
   - For each `d` in `domain.devices`: `uuid_to_name.get(&d.luks_uuid)` -> insert `(Unlocked, Some(d.underlying.clone()))`. This honors the LUKS UUID identity rule -- if the mapper name has drifted, the join still lands on the right disk.
   - For each `d` in `domain.null_underlying`: `persisted_devid_to_name.get(&d.devid)` -> insert `(Unlocked, None)`. Devid is the only persistent handle left when the backing device is gone.
   - Skip entries whose join misses; those flow to the fallback branch below and get a fresh `cryptsetup status` probe.
4. Build `disk_luks_states` for every declared disk in `disk_by_id`:
   - If the disk name is in `mounted_classification`: use that `(lock, underlying_present)`. No new cryptsetup status call -- the answer already lives in `domain`.
   - Otherwise (unmounted pool, or a mounted-but-unclassified disk such as one missing from btrfs filesystem show): run the **ownership-aware fallback** below to avoid trusting `cryptsetup status braid-{name}` solely on mapper basename.
   - For each disk regardless of branch: best-effort `CmdRequest::CryptsetupLuksDump { device: by_id }` for metadata. Failure maps to `metadata: None`.
5. **Ownership-aware fallback for one declared disk** (mirrors the canonical classifier at `cli/src/luks.rs:816-896`, mapped to the TUI's 4-way enum instead of `MapperOwnership` + `OwnershipError`):
   1. Run `CmdRequest::CryptsetupStatus { mapper: MapperName(format!("braid-{name}")) }` and parse via `parse_cryptsetup_status`.
   2. `is_active = false` -> `lock = Locked, underlying_present = None`.
   3. `is_active = true` with `device = None` or `Some("(null)")` -> `lock = Unknown, underlying_present = None`. Identity is unverifiable without btrfs's devid attestation, so the fallback refuses to claim the disk is open -- this matches the canonical classifier's `OwnershipError::Conflict { found: None }` semantic at `cli/src/luks.rs:837-845`. `Unlocked + underlying_present = None` is reserved for mounted `domain.null_underlying` entries that pre-populate `mounted_classification` via the persisted-devid identity join (step 3 above).
   4. `is_active = true` with `device = Some(path)`:
      - Canonicalize both `path` (the live underlying) and the configured `disk_by_id[name]` through a `&dyn BackingPathResolver` (`cli/src/luks.rs:702-715`). If they resolve to different block devices, classify as `lock = Unknown, underlying_present = Some(path)` -- the mapper at `braid-{name}` is open against a different physical disk than the one declared in membership.
      - Otherwise run `CmdRequest::CryptsetupLuksUuid { device: path }`; if the parsed UUID differs from `disk_luks_uuid[name]`, classify as `lock = Unknown, underlying_present = Some(path)`. The backing block device is the configured one but its LUKS header has the wrong UUID (drift or hostile reformat).
      - If both checks succeed -> `lock = Unlocked, underlying_present = Some(path)`.
   5. Any parse error or command failure in this chain -> `lock = Unknown`, `underlying_present` carries whatever the cryptsetup status reported (`Some(path)` if we got that far, else `None`).
6. If `!domain.mounted`, return `Ok((disk_luks_states, None))`.
7. Otherwise build the rest of `PoolState` as today and return `Ok((disk_luks_states, Some(pool_state)))`.

The expected mapper-name `braid-{name}` only ever drives the fallback `cryptsetup status` shell-out, and the fallback is then ownership-keyed before trusting the result. Identity (LUKS UUID, persisted devid) is always preferred when the mounted-pool probe has already done the work.

Threading the resolver: add a `&dyn BackingPathResolver` parameter to `probe_pool_for_tui` (sibling of the existing `&F: Filesystem` parameter). The `Effect::ProbePool` worker in `cli/src/tui/effect.rs` constructs a `RealBackingPathResolver` (`cli/src/luks.rs:709`) per spawn and passes it in. No new trait is introduced.

Reuse existing helpers:

- `parse_cryptsetup_status` (`cli/src/parse/cryptsetup_status.rs:35`).
- `parse_cryptsetup_luks_dump` (existing call at `cli/src/tui/probe.rs:137`).
- The mapper-name formatter pattern at `cli/src/tui/probe.rs:763` (`MapperName("braid-toshiba".into())`).

Drop the now-redundant `luks_info` HashMap population (cleanup commit only -- in the additive commit, leave it in place to keep snapshot diffs reviewable).

### `cli/src/tui/effect.rs`

Update the worker thread that maps `probe_pool_for_tui`'s result into `Event::PoolProbeFinished`. The Event payload changes shape from `Result<Option<PoolState>, String>` to `Result<(HashMap<String, DiskLuksState>, Option<PoolState>), String>`.

### `cli/src/tui/event.rs`

Update the `Event::PoolProbeFinished` variant's payload and the `into_message` mapping.

### `cli/src/tui/app.rs`

Update `Message::PoolProbeFinished` payload and reducer (`cli/src/tui/app.rs:140-178`):

- On `Ok((disk_luks_states, pool_opt))`: always write `model.disk_luks_states = disk_luks_states`. Then map `pool_opt` to `PoolStatus::Mounted(pool) | PoolStatus::NotMounted` as today.
- On `Err(e)`: leave `model.disk_luks_states` untouched (preserve last successful read, parallel to `ErrorStale` pool semantics).

### `cli/src/tui/view/mod.rs`

Replace `cli/src/tui/view/mod.rs:1098-1102`:

```rust
let state = model.disk_luks_states.get(&disk_name);
let lock_label = match state.map(|s| &s.lock) {
    Some(DiskLockState::Unlocked) => "unlocked",
    Some(DiskLockState::Locked) => "locked",
    Some(DiskLockState::Unknown) | None => "unknown",
};
// Render an additional "underlying device gone" line when the mapper
// is open but `cryptsetup status` reports no backing device.
let show_underlying_gone = matches!(
    state,
    Some(DiskLuksState { lock: DiskLockState::Unlocked, underlying_present: None, .. })
);
```

Use `state.and_then(|s| s.metadata.as_ref())` for the cipher/keyslot block instead of `pool.and_then(|p| p.luks_info.get(...))`.

Keep the Allocations table and btrfs device errors gated on `pool.current()` -- those are pool-membership concerns.

When `show_underlying_gone`, push a line `Span::styled("underlying device gone", Style::default().fg(Color::Yellow))` between the `Status` line and `Cipher` line.

### `cli/src/tui/demo.rs` and `cli/src/tui/mod.rs::run_demo`

`disk_luks_states` lives on `Model`, not on `PoolState`, so the existing `luks_info` fixture inside `sample_pool()` (`cli/src/tui/demo.rs:100-125`) cannot populate it -- moving the contents in-place would silently drop cipher/keyslot text from `braid tui --demo` and from any mounted disk-detail snapshot. Wire the demo at the model layer instead:

- Add `pub(crate) fn sample_disk_luks_states() -> HashMap<String, DiskLuksState>` to `cli/src/tui/demo.rs`. Returns one entry per demo disk (toshiba/ironwolf/wdc) with `lock = Unlocked`, a synthetic `underlying_present` (e.g. `Some("/dev/disk/by-id/...".into())`), and the same cipher/keyslot metadata the old `luks_info` fixture used (`aes-xts-plain64`, 512-bit, 1 keyslot).
- Initialize `disk_luks_states: HashMap::new()` in `Model::new_demo` (parallel to how `session_temperature_stats` is initialized empty).
- In `tui::run_demo` (`cli/src/tui/mod.rs:68-75`), after constructing the model assign `model.disk_luks_states = demo::sample_disk_luks_states();` before calling `run_with_model`.
- In every snapshot test that exercises a mounted disk detail (today: `snapshot_disk_detail` at `cli/src/tui/view/mod.rs:1531`), seed `model.disk_luks_states = demo::sample_disk_luks_states()` after `Model::new_demo` so the popup still renders the cipher/keyslot block.
- In the cleanup commit, drop the `luks_info` map from `sample_pool()`.

### Existing tests requiring fixture updates

- `cli/src/tui/view/mod.rs:1531` `snapshot_disk_detail` -- snapshot will pick up the new field plumbing but the rendered text shouldn't change (mounted + unlocked case unchanged). Re-record the snapshot only if the text actually moves.
- Probe-side tests at `cli/src/tui/probe.rs:735+`, especially the ones that already stub `CmdRequest::CryptsetupStatus` for pool members. The mounted-pool probe tests won't need new mocks (reuse domain.devices). The unmounted-state tests, if any, will need `CryptsetupStatus` outputs for each declared disk -- expect ~3-4 tests to need fixture additions.

## New tests

Behavioral / structure-insensitive coverage, ordered by what they protect:

1. **View snapshot: `snapshot_disk_detail_unmounted_mixed`** (`cli/src/tui/view/mod.rs`). Construct a `Model` with `PoolStatus::NotMounted` and a `disk_luks_states` map containing one `Unlocked` disk with metadata and one `Locked` disk. Render the popup against the first disk, then the second; assert both renderings via snapshot. Catches the original bug -- in the current code path, both render as `locked`.

2. **View snapshot: `snapshot_disk_detail_null_underlying`** (`cli/src/tui/view/mod.rs`). Construct a `Model` with `PoolStatus::Mounted(sample_pool())` and `disk_luks_states[selected] = DiskLuksState { lock: Unlocked, underlying_present: None, metadata: Some(_) }`. Pool must be `Mounted` because `Unlocked + underlying_present = None` is only legitimately produced via the mounted `domain.null_underlying` path (the fallback maps null-underlying to `Unknown`). Snapshot asserts `Status: unlocked` and `underlying device gone` both render.

3. **Reducer: `pool_probe_err_preserves_disk_luks_states`** (`cli/src/tui/app.rs` or a sibling test module). Seed `model.disk_luks_states` with one entry, then dispatch `Message::PoolProbeFinished(Err("transient"), _)`. Assert the map is unchanged. Protects the "transient probe failure shouldn't wipe diagnostic state" invariant in the reducer.

4. **Reducer: `pool_probe_ok_none_installs_fresh_disk_luks_states`** (same module). Dispatch `Message::PoolProbeFinished(Ok((HashMap::from([("d1", ...)]), None)), _)`. Assert both `model.pool == NotMounted` and `model.disk_luks_states["d1"]` is the new value. Protects the most common diagnostic path.

For every probe test below that asserts an `Unlocked` outcome, the ownership-aware fallback requires (a) backing-path canonicalization to succeed and agree, and (b) the underlying's LUKS UUID to match the configured one. Each such test wires a `MockBackingPathResolver` (`cli/src/test_fixtures/shared.rs:197-222`) seeded via `.with_path(by_id, kernel_path)` plus a matching `CmdRequest::CryptsetupLuksUuid { device: kernel_path }` stub. Without that wiring the fallback would classify as `Unknown` for harness reasons, not for the asserted reason.

5. **Probe unit: `probe_classifies_unmounted_open_and_closed_mappers`** (`cli/src/tui/probe.rs`). Drive the unmounted state through the `Filesystem` trait, not `BtrfsFilesystemShow`: `probe_pool` reads mount state from `/proc/self/mountinfo` via `fstype_at_mount_via_fs` (`cli/src/mount_check.rs:185`), so the test sets up `MockFs::with_mountinfo(&mountinfo_without_target())` -- the same pattern as `probe_pool_unmounted` at `cli/src/probe.rs:1230-1238`. Two declared disks: `open` (active) and `closed` (inactive). MockRunner stubs: `CryptsetupStatus { mapper: braid-open }` active + `device = "/dev/vdb"`; `CryptsetupStatus { mapper: braid-closed }` inactive; `CryptsetupLuksUuid { device: "/dev/vdb" }` returning `disk_luks_uuid["open"]`; `CryptsetupLuksDump` outputs for both with distinct cipher strings. `MockBackingPathResolver::default().with_path(disk_by_id["open"], "/dev/vdb")` so the canonicalization branch agrees. Assert (a) the returned tuple's `pool` is `None`, (b) `disk_luks_states["open"].lock = Unlocked` with metadata, and (c) `disk_luks_states["closed"].lock = Locked`.

6. **Probe unit: `probe_status_active_metadata_failed_decouples_lock_and_metadata`** (`cli/src/tui/probe.rs`). MockRunner: `CryptsetupStatus` active with `device = "/dev/vdb"`; `CryptsetupLuksUuid { device: "/dev/vdb" }` returning the configured UUID for that disk; `CryptsetupLuksDump` fails. `MockBackingPathResolver` seeded with the matching by-id -> `/dev/vdb` pair. Assert `lock = Unlocked`, `underlying_present = Some("/dev/vdb")`, `metadata = None`. Protects the previously-implicit coupling between "mapper open" and "metadata available."

7. **Probe unit: `probe_fallback_classifies_foreign_uuid_mapper_as_unknown`** (`cli/src/tui/probe.rs`). Unmounted-state filesystem (same setup as test #5). MockRunner: `CryptsetupStatus { mapper: braid-{name} }` active + `device = "/dev/vdb"`; `CryptsetupLuksUuid { device: "/dev/vdb" }` returning a UUID that does NOT match `disk_luks_uuid[name]`. `MockBackingPathResolver::default().with_path(disk_by_id[name], "/dev/vdb")` -- the canonicalization branch agrees, isolating the UUID-mismatch branch. Assert `disk_luks_states[name].lock = Unknown`. Protects the project invariant that mapper basename never authoritatively identifies a disk -- ownership comes from LUKS UUID + by-id, never from `braid-{name}`.

Tests intentionally NOT added:

- A test that the mounted-pool path doesn't issue duplicate `CryptsetupStatus` calls -- that's an internal optimization detail. The reuse-from-domain branch is unit-exercised by the mounted-pool probe tests already in the suite.

## Out of scope

- `unpooled_disks` classification in the popup (header damaged / unknown LUKS / wrong-version). That's a different concern -- pool-membership state, not LUKS state -- and currently rendered in the disk *table*, not the popup. Same "lost when unmounted" architectural shape, but deserves its own pivot.
- Adding a separate lock-state for hot-unplug. Handled instead by the orthogonal `underlying_present` axis.
- Probing additional LUKS fields (label, slot identifiers, header backup path) -- existing `DiskLuksInfo` shape suffices.

## Verification

- `just test-rust` -- runs the new probe units, reducer tests, and view snapshots.
- `just test-parsers` -- live CLI parser canary against actual `cryptsetup status` / `cryptsetup luksUUID` / `cryptsetup luksDump` output in a VM. This is the right "live tools" smoke for this change; there is no VM test that drives `braid tui` itself (the TUI guards on `require_tty` at `cli/src/tui/mod.rs:28`), so live TUI-path validation is via the manual steps below, not via `just test-vm`.
- Manual end-to-end. Note: the pool probe does not auto-refresh today -- `Message::PoolProbeFinished` returns no schedule effect (`cli/src/tui/app.rs:172-177`, TODO marker), so the operator must press `r` to re-probe after every external state change.
  1. **Unmounted, all locked.** On a NixOS test VM with a configured but unmounted pool, run `braid tui`. Open the disk detail popup (Enter on a disk). Confirm `Status: locked` and cipher/keyslot info both render.
  2. **Unmounted, one mapper open with real underlying.** From a separate shell, `cryptsetup open` one of the LUKS devices using the configured key (mapper open, but pool stays unmounted -- skip the mount step). Press `r` in the TUI to refresh. Confirm the open disk now shows `Status: unlocked` while the others remain `locked`. (This exercises the ownership-aware fallback's success branch -- backing path canonicalizes and UUID matches.)
  3. **Unmounted, foreign mapper.** Close the mapper, then `cryptsetup open` an unrelated LUKS device under the same `braid-<name>` mapper name. Press `r`. Confirm `Status: unknown` for that disk. (Exercises the fallback's UUID-mismatch branch.)
  4. **Mounted, hot-unplug.** Close the foreign mapper, unlock + mount the pool normally, and press `r`; confirm Allocations and btrfs Device Errors tables render as today (regression check for the unchanged path). Then simulate hot-unplug of one mounted member (e.g. `echo 1 > /sys/block/<dev>/device/delete`). Press `r`. Confirm `underlying device gone` renders for that disk -- this hits the mounted `domain.null_underlying` path, which is the only path that grants `Unlocked + underlying_present: None`.
