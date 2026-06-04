# Plan: Browse SMART picker uses live backing paths for present members

## Context

Decision 024 ("LUKS UUID Is Disk Identity") states the invariant (current
wording as of `1de8f614`):

> **Present-device probes use live paths.** Queries such as lsblk model/serial
> and smartctl use the live backing path (`PoolState::underlying_for_uuid`), and
> the TUI disk-detail LUKS metadata dump (`cryptsetup luksDump`) reads the live
> backing path for a verified-present (`Unlocked`) member -- not persisted by-id
> setup/repair handles that can drift while the disk is still present.

Two commits already moved present-device surfaces onto the live path: `8593c9fe`
("query present hardware through live paths") for `status`/`doctor` model/serial
and the **Data-tab** TUI SMART/temperature probe, and `1de8f614`
("read disk-detail luks metadata from the live backing path") for the disk-detail
`luksDump` dump. The **Browse-tab** SMART device picker is the one remaining
non-compliant surface. `BrowseState::populate_smartctl_devices`
(`cli/src/tui/browse/state.rs`) still builds its picker rows straight from
`disks.by_id` and `selected_smartctl_request` dispatches `smartctl` against that
by-id path for **every** disk, including present pool members.

Consequence: for a present member whose by-id path has drifted (enclosure /
controller renaming, etc.), `Browse > SMART` probes a stale-or-wrong device
while the Data tab probes the correct live node -- the two surfaces disagree.
This is the exact failure mode Decision 024 + `8593c9fe` set out to eliminate,
so the Browse picker currently **contradicts an Active ADR**.

Severity is Low (by-id drift on a *present, mounted* member is uncommon), but
the fix is small and brings the last SMART surface into compliance.

The live backing path already exists at probe time: `probe_pool_for_tui`
computes a per-name `mounted_classification` map whose value is
`Some(device.underlying)` for UUID-identified present members and uses it for
the Data-tab SMART loop. That `underlying` value is **not** lost -- it reaches
the Model as `disk_luks_states[name].underlying_present` -- but it never reaches
the TUI `PoolState`, and `underlying_present` is the **wrong** map to resolve
SMART from: it is a *superset* of `mounted_classification`. For a member whose
mapper is open with correct backing+UUID but which is not assembled into the
live btrfs pool (degraded mount), `build_disk_luks_states` takes the
`fallback_disk_luks_lock` path and yields `(Unlocked, Some(path))`, so
`underlying_present` is populated while `mounted_classification` has no entry.
Resolving the Browse picker from `underlying_present` would therefore give such
a member a live path while the Data-tab SMART loop (keyed on
`mounted_classification`) gives it by-id -- silently reintroducing the very
divergence this fix exists to kill.

The ideal fix persists the `mounted_classification`-sourced subset on
`PoolState` as a dedicated field and resolves both SMART surfaces from that one
source. The cross-surface parity guard is the shared *source* (one
`present_underlying` assigned to both the Data-tab loop and the new field),
pinned by a probe test on the field's contents -- not the one-line lookup fn.

## Decisions (locked with user)

- **Picker display:** show the *resolved* device (live path for present members,
  by-id for offline) and rename the picker column header `ByIdPath` -> `Device`.
  What the row shows is exactly what `smartctl` will probe -- no table-vs-command
  divergence.
- **Test coverage:** flip the present-disk assertion in the VM canary to the live
  path; cover both branches (present -> live, offline -> by-id) with fast,
  structure-insensitive Rust unit tests. Do **not** add an offline disk to the
  VM scenario (redundant coverage at integration cost).

## Approach

One shared resolver + one persisted map; no `DiskInventory` widening (because
`pool: &PoolStatus` is already threaded to every Browse entry point).

1. **Single resolver (one definition, in `model.rs`).** Add a free fn next to
   `PoolState` in `cli/src/tui/model.rs`. Both callers (`probe.rs` and
   `browse/state.rs`) already `use crate::tui::model::…`, so defining it here
   keeps the existing probe->model / browse->model edges and avoids a needless
   model->probe back-edge (which a delegating `PoolState` method would force,
   since `model.rs` imports nothing from `probe.rs`). No method on `PoolState`.

   ```rust
   /// Device a hardware (SMART) probe must target for a disk: the live backing
   /// path when the member is present, else the persisted by-id handle
   /// (decision 024). Both SMART surfaces call this with a
   /// `mounted_classification`-sourced map, so they resolve identically.
   pub(crate) fn smart_query_device<'a>(
       name: &str,
       by_id: &'a str,
       present_underlying: &'a HashMap<String, String>,
   ) -> &'a str {
       present_underlying.get(name).map(String::as_str).unwrap_or(by_id)
   }
   ```

   The parity guard is not this lookup but the shared *source*: the probe assigns
   one `present_underlying` map both to the Data-tab SMART loop and to
   `PoolState.disk_underlying` (step 3), and a probe test pins the field's
   contents.

2. **Persist the live paths on the TUI `PoolState`.** Add
   `disk_underlying: HashMap<String, String>` with this doc:

   > Live backing block-device path per **btrfs-assembled, UUID-verified**
   > present member -- the `mounted_classification` subset, sourced identically
   > to the Data-tab SMART loop. Deliberately **not**
   > `disk_luks_states.underlying_present`, which is a superset (it also covers
   > `fallback_disk_luks_lock`-classified open mappers not assembled into btrfs);
   > resolving SMART from the superset would reintroduce the Data-tab-vs-Browse
   > divergence this field exists to prevent. Absent => caller falls back to
   > by-id.

   It is the TUI-layer, name-keyed analog of the domain's
   `PoolState::underlying_for_uuid`.

3. **Probe populates it and uses the resolver.** In `probe_pool_for_tui`
   (`cli/src/tui/probe.rs`), after building `mounted_classification`, derive
   `present_underlying` from its `Some(underlying)` entries, rewrite the Data-tab
   SMART loop to call `smart_query_device(name, by_id, &present_underlying)`
   (imported from `model`; replacing the current inline
   `.get(...).and_then(...).unwrap_or(...)`), and store `present_underlying` into
   the returned `PoolState.disk_underlying`.

4. **Browse picker resolves through the same rule.** In
   `populate_smartctl_devices`, resolve each row's device via the pool:
   present member -> live path, offline/no-pool -> by-id. Store the *resolved*
   device as the row's second tuple element. `selected_smartctl_request` and the
   footer path (`command_display` -> `selection_request`) are unchanged -- they
   already read the stored device, so they now show/dispatch the resolved one.

## Changes by file

- **`cli/src/tui/model.rs`** -- add `disk_underlying: HashMap<String, String>`
  to `struct PoolState` (with the doc above), and the free `smart_query_device`
  resolver next to it. No method on `PoolState` (keeps `model` free of a `probe`
  back-edge).

- **`cli/src/tui/probe.rs`** -- derive `present_underlying` from
  `mounted_classification`; call `smart_query_device` (imported from `model`) in
  the SMART loop (replacing the current inline `query_device` computation); set
  `disk_underlying` in the `PoolState { .. }` literal it returns.

- **`cli/src/tui/browse/state.rs`** -- change
  `fn populate_smartctl_devices(&mut self, disks: &DiskInventory<'_>, pool: &PoolStatus)`;
  resolve each device via `smart_query_device` from `model`
  (`pool.current()` => `smart_query_device(name, by_id, &p.disk_underlying)`,
  else by-id) and store the resolved path. Pass `pool` at its three internal
  call sites:
  `load_current` (smartctl-picker arm), `enter` (re-populate arm),
  `reload_detail` (`SmartctlDeviceDetail` arm) -- all already have `pool` in
  scope. Import the resolver. `DiskInventory`, its construction sites, and
  `cli/src/tui/app.rs` are untouched.

- **`cli/src/tui/browse/view.rs`** -- in `render_smartctl_device_table`, rename
  the header cell `"ByIdPath"` -> `"Device"`.

- **TUI `PoolState` literal fixtures** (compiler-enforced; add the new field):
  `cli/src/tui/probe.rs` (production return -- real value), and the test/demo
  fixtures `cli/src/tui/demo.rs#sample_pool`, `cli/src/tui/app.rs#pool_with_temperature`,
  `cli/src/tui/browse/state.rs` (`pool()` helper), `cli/src/tui/view/mod.rs#pool_with_df_entries`.
  Set the demo/test fixtures to `HashMap::new()` (empty = fallback path) except
  where a test needs a populated map. Note: the many other `PoolState { .. }`
  literals in `status.rs`/`replace.rs`/`lock.rs`/etc. are the **domain**
  `PoolState` in `cli/src/types.rs` -- unrelated, do not touch.

- **`docs/design/decisions/024-luks-uuid-identity.md`** -- add one
  "Tests That Enforce This" bullet noting the TUI Browse SMART picker now
  resolves present members through the live path. No other ADR change: it
  already mandates this behavior; we are making code comply.

## Tests

- **Add (browse/state.rs, behavioral, structure-insensitive):** a `pool()` whose
  `disk_underlying` maps `disk1 -> /dev/vdb`, with a by-id inventory for
  `disk1`/`disk2`. After `load_current` on a SMART picker selection, assert the
  resolved row / footer for `disk1` is the **live** path (`smartctl -H /dev/vdb`)
  and `disk2` (absent from `disk_underlying`) **falls back to by-id**. This pins
  both branches in one test.

- **Keep (browse/state.rs):** the two existing tests that load picker rows from
  a disk inventory and assert the by-id device --
  `command_display_smartctl_picker_preview_uses_selected_device` (asserts
  `smartctl -H /dev/disk/by-id/virtio-disk1`) and
  `enter_in_smartctl_device_row_drills_in` (asserts `/dev/disk/by-id/virtio-disk2`)
  -- now run against an **empty** `disk_underlying` (fixture default) and
  therefore assert the by-id **fallback**, still correct. Add a one-line comment
  marking them as the no-live-path case.

- **Add (probe.rs):** assert a present-but-closed / offline member is **absent**
  from `PoolState.disk_underlying` (so Browse falls back to by-id). Can extend
  the existing `smartctl_health_for_present_member_uses_live_underlying` to also
  assert `disk_underlying` contents for one present + one offline member.

- **Regenerate (insta):** `snapshot_browse_smartctl_health_picker` (header
  `ByIdPath` -> `Device`). The `_detail` snapshot is unaffected while demo
  `disk_underlying` stays empty (footer device == by-id). Follow
  `docs/dev/tui-snapshots.md`.

- **Flip (VM canary `tests/cli/braid-tui-browse.py`):** the SMART-detail
  assertion `wait_until_tty_matches("2", r"/dev/disk/by-id/")` ->
  `r"/dev/vd"`. `disk1` is a present, unlocked member, so after the fix its
  detail/footer shows the live backing node (expected `/dev/vdb` from
  `cryptsetup status braid-disk1`). The `r"disk1"` waits above/below are
  unchanged. Confirm the exact node by running the test; widen/narrow the regex
  if the VM enumerates differently.

## Verification

1. `just test-rust` -- runs the new + existing browse/probe unit tests and the
   insta snapshot assertions. Accept the regenerated picker snapshot
   (`cargo insta review` or the flow in `docs/dev/tui-snapshots.md`).
2. `just test-vm braid-tui-browse` -- focused VM run; confirms the real
   end-to-end present path (real `cryptsetup status` -> live node -> dispatched
   `smartctl`) renders the live `/dev/vd*` device for `disk1`.
3. No parser-critical tool-version change and no `flake.lock` nixpkgs bump, so
   **no fixture refresh** is required.

This is a localized TUI change (Browse picker + probe plumbing); a full
`just test-vm` is not warranted -- hand back to the user for any broader run.

## Scope / non-goals

- **Only** the TUI Browse SMART picker. `status`/`doctor` model/serial and the
  Data-tab SMART probe are already compliant (`8593c9fe`).
- The disk-detail `luksDump` metadata dump is **already** routed through the live
  path for present (`Unlocked`) members by `1de8f614`
  (`build_disk_luks_states`); it is not a non-goal because it is non-compliant,
  but because it is already done. The Browse SMART picker is the one remaining
  by-id present-device surface.
- Browse `lsblk` views are whole-system invocations, not per-disk by-id probes
  -- unaffected.
- No `DiskInventory` widening, no `app.rs` change, no migration shim (unreleased
  software).

## Implementation notes

- The plan's fixture list named `cli/src/tui/app.rs#pool_with_temperature` and
  `cli/src/tui/view/mod.rs#pool_with_df_entries` among the `PoolState` fixtures
  to extend, but both are helper fns that build on `sample_pool()` and mutate a
  single field, so they inherit `disk_underlying` automatically. Only the three
  true `PoolState { .. }` literals needed the new field -- `probe.rs` (production
  return), `demo.rs#sample_pool`, and `browse/state.rs` `pool()`. The compiler
  confirmed exactly those three (E0063); the two derived helpers were left
  untouched.
- The Data-tab SMART loop's old inline resolution
  (`mounted_classification.get(name).and_then(|(_, u)| u.as_deref()).unwrap_or(by_id)`)
  is behaviorally identical to `smart_query_device(name, by_id, &present_underlying)`
  because `present_underlying` is exactly the `Some(underlying)` subset of
  `mounted_classification` -- so swapping it in is a pure refactor of that loop,
  not a behavior change.
