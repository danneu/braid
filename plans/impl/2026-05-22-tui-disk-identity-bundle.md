# Group membership-derived disk identity fields on the TUI Model

## Context

A code-review finding flagged `Model.disk_devid` (`cli/src/tui/model.rs:284`) as
carrying a field that exists only to feed `Effect::ProbePool`, widening the
`Model::new` constructor (`#[allow(clippy::too_many_arguments)]` at
`cli/src/tui/model.rs:311`) for no view/browse/keymap benefit. The diagnosis is
correct but the finding's prescription -- move just `disk_devid` into a
probe-only context -- is a half-measure:

- `Model.disk_luks_uuid` has the exact same property (only read at
  `cli/src/tui/app.rs:109` to clone into `Effect::ProbePool`). Singling out
  `disk_devid` is inconsistent.
- `Model::new` takes nine arguments; removing one leaves it at eight, still
  above clippy's threshold of seven, so the `#[allow]` (which the finding
  cited as part of the impact) would survive.
- All four membership-derived disk fields -- `disk_names`, `disk_by_id`,
  `disk_luks_uuid`, `disk_devid` -- come from the same
  `membership.iter_by_name()` pass at `cli/src/tui/mod.rs:36-55` and never
  mutate during the TUI's lifetime. They are one logical bundle that has been
  carried as four parallel fields.

The intended outcome: collapse those four fields into a single
`DiskIdentity` value on `Model`, thread it through `Effect::ProbePool` as a
single payload (replacing the three cloned maps in the effect), and drop the
`#[allow(clippy::too_many_arguments)]`. This is the same pattern already used
elsewhere in the codebase for per-disk metadata bundles (`DiskHwInfo` at
`cli/src/confirm.rs:62-67`, `DiskLuksState` at `cli/src/tui/model.rs:68-74`).

## Design

### New type

Add to `cli/src/tui/model.rs`, near the other small bundle structs:

```rust
/// Membership-derived disk identity bundled as one value so the TUI model and
/// probe effect share a single source of truth instead of four parallel maps.
/// All fields are name-keyed; `names` carries display order from
/// `membership.iter_by_name()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskIdentity {
    pub names: Vec<String>,
    pub by_id: HashMap<String, String>,
    pub luks_uuid: HashMap<String, LuksUuid>,
    /// Persistent btrfs devid bindings, used when a live probe cannot observe
    /// the underlying LUKS UUID for a mounted device.
    pub devid: HashMap<String, u64>,
}

impl DiskIdentity {
    /// Build the TUI's name-keyed view of pool membership at session start.
    pub fn from_membership(m: &crate::membership::PoolMembership) -> Self { ... }
}
```

`from_membership` does the same iteration currently inlined at
`cli/src/tui/mod.rs:36-55`, returning the four parts in one struct. `Default`
gives the demo path an empty identity for free.

### Model changes (`cli/src/tui/model.rs`)

- Replace the four fields (`disk_names`, `disk_by_id`, `disk_luks_uuid`,
  `disk_devid`) with a single `pub disks: DiskIdentity`.
- `Model::new`: replace the four `Vec`/`HashMap` arguments with one
  `disks: DiskIdentity`. The constructor drops from 9 args to 6, so remove
  `#[allow(clippy::too_many_arguments)]`.
- `Model::new_demo`: keep its current `disk_names` parameter but populate
  `disks: DiskIdentity { names: disk_names, ..Default::default() }` -- the
  demo path stays trivial for the ~40 test call sites that pass only names.

### Entry-point change (`cli/src/tui/mod.rs:29-67`)

Collapse the inline `disk_names` / `disk_by_id` / `disk_luks_uuid` /
`disk_devid` builders (lines 36-55) into a single call:

```rust
let disks = DiskIdentity::from_membership(&membership);
let (model, init_effects) = Model::new(
    disks,
    config.mount_point().0.clone(),
    config.fan_control().cloned(),
    config.ups().cloned(),
    advisories,
    paths.clone(),
);
```

### Effect change (`cli/src/tui/effect.rs:17-27, 53-80`)

`Effect::ProbePool` payload becomes:

```rust
ProbePool {
    mount_point: MountPoint,
    disks: DiskIdentity,
    paths: StatePaths,
},
```

(Three fields replace the previous five.) `execute_effect` passes `&disks`
through to `probe_pool_for_tui` as a single argument -- no destructure at
the effect layer. `Effect::ProbeFan` is **not** changed -- its actual
dependency is just `disk_by_id`, and broadening its payload to the full
identity would be over-sharing.

### Probe signature (`cli/src/tui/probe.rs:141-160`)

`probe_pool_for_tui`'s three separate map arguments (`&disk_by_id`,
`&disk_luks_uuid`, `&disk_devid`) collapse to a single `&DiskIdentity`
parameter. The probe body destructures the maps it needs internally (the
existing `uuid_to_name` build at `probe.rs:153-156` and
`persisted_devid_to_name` build at `probe.rs:157-160` read
`disks.luks_uuid` and `disks.devid` directly). The probe is the sole
consumer of those three maps in concert, so this is the only place
destructuring is needed.

Update the ~14 `probe_pool_for_tui` test call sites
(`cli/src/tui/probe.rs:1057, 1215, 1333, 1465, 1562, 1632, 1686, 1815, 1899,
1970, 2052, 2124, 2203, 2282` and friends) to build a `DiskIdentity` literal
in their fixture helpers. `tui_disk_devid()` at `probe.rs:936` becomes
`tui_disks() -> DiskIdentity`.

### Refresh path (`cli/src/tui/app.rs:106-112`)

Replace the three `model.disk_X.clone()` lines with one:

```rust
effects.push(Effect::ProbePool {
    mount_point: model.mount_point.clone(),
    disks: model.disks.clone(),
    paths,
});
```

### Read-site updates

Mechanical renames across the TUI. Pattern: `model.disk_names` ->
`model.disks.names`, `model.disk_by_id` -> `model.disks.by_id`, etc.
Representative sites (full list via `rg "model\.disk_(names|by_id|luks_uuid|devid)" cli/src/tui/`):

- `cli/src/tui/view/mod.rs:203, 737, 811, 820, 832, 844, 861, 934, 1078` --
  `disk_names` reads for table rendering, height, and selection lookup.
- `cli/src/tui/app.rs:62, 136, 143, 179, 194, 335` -- fan probe builder,
  selection bounds, command builder borrows.
- `cli/src/tui/browse/view.rs:401, 405, 591, 864, 929, 948, 990` -- one
  mutating test helper (`seed_disk_by_id`) and several `&model.disk_by_id`
  borrows passed into command builders.

The single test helper at `browse/view.rs:401, 405` that mutates
`model.disk_by_id.insert(...)` becomes `model.disks.by_id.insert(...)` --
trivial.

### Test-site updates

`Model::new_demo(sample_disk_names(), pool)` keeps the same call shape across
all ~40 test sites in `cli/src/tui/app.rs`, `cli/src/tui/view/mod.rs`, and
`cli/src/tui/browse/view.rs`. No signature change for callers of `new_demo`.

`Model::new` has both the production caller in `cli/src/tui/mod.rs` and four
test callers in `cli/src/tui/app.rs:480, 520, 545, 570, 594` (the startup /
scheduler-pending and fan/ups emission tests at
`startup_probes_without_scheduler_pending_then_first_finish_arms_loop` and
the three `Model::new` emission tests that follow). All five call sites
need updating to the new signature. The four test sites currently pass
three empty `HashMap::new()` arguments inline -- replace those three
arguments with a single `DiskIdentity::default()` (or, when a test needs
specific identity content, build the struct via `DiskIdentity { names:
sample_disk_names(), ..Default::default() }`).

### New behavioral coverage

The refactor moves a silent mapping step (the inline membership-to-maps
loop at `cli/src/tui/mod.rs:36-55`) into a named function and threads it
through `Effect::ProbePool`. A bad implementation could compile while
swapping `luks_uuid` and `devid`, dropping a field, or letting startup /
refresh emit a `DiskIdentity::default()` instead of the membership-derived
value. Add focused Rust unit coverage in `cli/src/tui/model.rs`'s test
module (or a sibling test module next to `DiskIdentity`):

1. **`from_membership_maps_all_four_fields`** -- build a `PoolMembership`
   with two members whose `DiskName` order is **inverted** relative to
   their `LuksUuid` order (e.g. UUID-A holds name `"zeta"`, UUID-B holds
   name `"alpha"`), and one member with `devid: Some(7)` while the other
   has `devid: None`. Call `DiskIdentity::from_membership(&m)` and assert:
   - `names` equals `["alpha", "zeta"]` (sorted by `DiskName`, matching
     `iter_by_name()`'s contract from decision 024).
   - `by_id["alpha"]` and `by_id["zeta"]` match the per-member `ByIdPath`.
   - `luks_uuid["alpha"] == UUID-B` and `luks_uuid["zeta"] == UUID-A` --
     name-to-UUID mapping is correct and not swapped with `by_id` or
     `devid`.
   - `devid` contains exactly one entry for the member with
     `devid: Some(7)` (the `None` member is absent, matching the
     `filter_map` shape at `mod.rs:48-55`).

2. **`new_carries_identity_into_initial_probe`** -- pass a non-empty
   `DiskIdentity` into `Model::new`. Pattern-match the returned `Vec<Effect>`
   for the initial `Effect::ProbePool` and assert its `disks` field equals
   the input -- guards against startup silently emitting a `Default`
   identity.

3. **`refresh_pool_carries_identity_into_probe`** -- construct a `Model`
   via `new_demo` with a non-empty `disks` (set after construction, mirror
   of the existing `seed_disk_by_id` helper pattern), set
   `model.paths = Some(...)`, dispatch `Message::RefreshPool` through
   `update`, and pattern-match the returned effects for an
   `Effect::ProbePool` whose `disks` matches the model's. Guards against
   the refresh path regressing to an empty / wrong identity.

These three tests are behavioral and structure-insensitive -- they
exercise the data-flow boundary the refactor introduces (membership ->
identity -> effect) without depending on field order, helper names, or
internal probe details.

## Critical files

- `cli/src/tui/model.rs` -- struct definition, `Model::new`, `Model::new_demo`.
- `cli/src/tui/mod.rs` -- entry-point that constructs `DiskIdentity` from
  membership.
- `cli/src/tui/effect.rs` -- `Effect::ProbePool` payload + worker destructure.
- `cli/src/tui/probe.rs` -- `probe_pool_for_tui` signature + ~14 test sites.
- `cli/src/tui/app.rs` -- refresh path clone + selection / fan probe
  read-sites.
- `cli/src/tui/view/mod.rs`, `cli/src/tui/browse/view.rs` -- renames only.

## Out of scope

- `Arc<DiskIdentity>` -- the indirection isn't justified at TUI scale (refresh
  on keypress, <20 entries), and it would complicate the `seed_disk_by_id`
  mutating test helper.
- `Effect::ProbeFan` payload changes -- fan probe's dependency is genuinely
  just `disk_by_id`; broadening it would be over-sharing.
- Replacing `PoolMembership` directly on `Model` -- `PoolMembership` is
  UUID-keyed, while every TUI read is name-keyed; name-keying the membership
  type to satisfy the TUI is a much larger change for no net win.
- Touching `cli/src/remove_missing.rs` `disk_devid` references -- those are
  test-fixture names (`two_disk_devids_pinned`, `three_disk_devids_pinned`)
  for the non-TUI `remove-missing` workflow, unrelated to this struct.

## Verification

1. **Compile**: `cargo check -p braid-cli` -- catches every renamed field
   reference.
2. **Unit tests**: `just test-rust` -- the ~14 `probe_pool_for_tui` test sites
   and the ~40 `Model::new_demo` test sites must all pass, plus the three
   new tests from "New behavioral coverage"
   (`from_membership_maps_all_four_fields`,
   `new_carries_identity_into_initial_probe`,
   `refresh_pool_carries_identity_into_probe`). Watch for golden parser
   fixtures: this refactor does not touch parsers, so no fixture refresh is
   needed.
3. **Targeted VM tests**: this is a TUI-internal refactor with no behavior
   change visible to the running NAS. Skip the full `just test-vm` and run
   only the smoke test that exercises TUI startup if one exists; otherwise no
   VM coverage is needed.
4. **Manual TUI walk**: `cargo run -p braid-cli -- tui` against a fixture
   pool. Confirm the disk table renders names in membership order, refresh
   (`r`) still triggers a probe and updates the table, and the browse tab's
   command builders show correct `by_id` paths. The `disk_devid` consumer is
   exercised when a member is mounted but its underlying LUKS UUID isn't
   visible to the live probe -- the existing probe tests cover that path, so
   passing `just test-rust` is the real signal.
5. **Lint**: `cargo clippy -p braid-cli` -- confirm the
   `#[allow(clippy::too_many_arguments)]` line at `model.rs:311` has been
   removed and clippy is silent.
