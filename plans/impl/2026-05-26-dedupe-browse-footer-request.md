# Dedupe Browse footer/request derivation (single source of truth)

## Context

`BrowseState::command_display` (the `$ ...` footer in the Browse TUI tab) and
`BrowseState::current_request` (what actually runs when you navigate to a
selection) are two large parallel `match` arms over the same
`BrowseSelection` space. Investigation confirmed:

- ~20 arms are duplicated verbatim (modulo `Some(...)` wrapping and
  `mount_point.clone()` vs `mount_point?.clone()`).
- The two matches differ in **exactly two intentional** places: NUT snapshot
  views (`command_display` shows `upsc <name>`, `current_request` returns
  `None` because the data comes from the model snapshot) and smartctl pickers
  (`command_display` shows a per-device preview, `current_request` returns
  `None` because the device list comes from the disk inventory).
- This duplication has **already produced a latent bug**: in subvolume
  drill-in (`BrowseMode::SubvolDetail`) the footer shows
  `btrfs subvolume list <mount>` instead of the `btrfs subvolume show <path>`
  that actually ran. `command_display` has detail-mode guards for smartctl and
  systemd (`state.rs:893-898`) but **none for subvol**, so subvol falls
  through to the list arm. smartctl/systemd detail footers are correct; subvol
  was missed.
- `command_display` has **zero test coverage**.

The original finding proposed "stash the last dispatched request and render the
footer from it." That is unsound as stated -- it would regress the smartctl
*picker* footer (Normal mode dispatches nothing; the footer is a live
per-device preview that tracks the selection cursor) and it misreads the
intentional NUT divergence as drift. The pivot below keeps the stash idea but
applies it **only to detail modes**, where a dispatch actually happens, and
derives the Normal-mode footer from a shared map instead.

Outcome: one selection->request map feeds both the navigate path and the
footer; detail-mode footers render exactly what ran; the subvol footer bug is
fixed as an inseparable consequence.

## Approach

All changes are in `cli/src/tui/browse/state.rs`.

### 1. Stash the dispatched drill-in request

Add a field to `BrowseState` (struct at `state.rs:406-438`, defaulted at
`:440-475`):

```rust
/// Footer source for detail modes: the request last dispatched into a
/// drill-in. The per-selection map only describes Normal-mode views, so the
/// `$ ...` footer renders this instead while drilled in -- guaranteeing it
/// matches what ran (and fixing subvol detail showing the list command).
last_detail_request: Option<CmdRequest>,
```

Set it inside the `dispatch()` method (`state.rs:1040-1050`), which is the
single funnel for all six drill-in dispatches (`enter` at `:616/625/631`,
`reload_detail` at `:668/675/679`) and is *not* used by Normal-mode navigation
(that uses the inline path at `load_current` `:540-546`):

```rust
self.last_detail_request = Some(request.clone());
```

`CmdRequest` derives `Clone` (`cli/src/cmd.rs:5`). No need to clear it on
`back()`; `command_display` only reads it while `mode != Normal`, and `back()`
resets `mode` to `Normal` (`:645`).

### 2. Extract the single selection->request map

New private method -- this is the body of the current `command_display` match
(`:899-1003`) minus the two detail-mode guards, made mount-point-optional so
both callers share it:

```rust
/// Single source of truth mapping a Normal-mode Browse selection to the
/// request that describes it. Both the navigate-dispatch path
/// (`current_request`) and the footer (`command_display`) derive from this
/// so a view's footer can never drift from the command it runs.
fn selection_request(
    &self,
    mount_point: Option<&MountPoint>,
    ups_config: Option<&Ups>,
) -> Option<CmdRequest> {
    let request = match self.current_selection() {
        // btrfs/lsblk arms: CmdRequest::Btrfs* { mount_point: mount_point?.clone() }
        // nut commands/clients/rwvars/upses arms: as today
        BrowseSelection::NutStatus | BrowseSelection::NutVariables => {
            CmdRequest::UpscQuery { name: ups_config?.name.clone() }
        }
        BrowseSelection::SystemdStatus | BrowseSelection::SystemdShow => {
            CmdRequest::SystemctlListUnitsBraidJson
        }
        // systemd braid/failed/timers/mounts, SmartctlScan, lsblk: as today
        BrowseSelection::SmartctlHealth
        | BrowseSelection::SmartctlInfo
        | BrowseSelection::SmartctlAttributes
        | BrowseSelection::SmartctlSelftestLog
        | BrowseSelection::SmartctlErrorLog => return self.selected_smartctl_request(),
        // ...
    };
    Some(request)
}
```

The btrfs arms gain `mount_point?` (was unconditional in `command_display`
because it took `&MountPoint`; now `Option`). For `command_display` the value
is always `Some`, so rendering is unchanged; for `current_request` an offline
pool yields `None`, matching today's `mount_point?.clone()`.

`selected_smartctl_request` (`:1170-1190`) and `selected_systemd_request`
(`:1192-1203`) are kept -- still used by `enter`/`reload_detail`, and
`selected_smartctl_request` is now also the smartctl-picker preview source
inside `selection_request`.

### 3. `current_request` delegates, nulling the two navigate exceptions

```rust
fn current_request(&self, pool: &PoolStatus, ups_config: Option<&Ups>) -> Option<CmdRequest> {
    let selection = self.current_selection();
    // Navigation does not dispatch snapshot-backed views (NUT status/variables
    // render from the model snapshot) or the smartctl picker (its rows come from
    // the disk inventory). `load_current` short-circuits both before reaching
    // here; this guard keeps the contract explicit.
    if selection.uses_model_snapshot() || selection.is_smartctl_picker() {
        return None;
    }
    self.selection_request(pool.current().map(|p| &p.mount_point), ups_config)
}
```

`uses_model_snapshot()` and `is_smartctl_picker()` already exist
(`:376-393`). This reproduces today's `current_request` exactly: those are the
only two selection groups where it returned `None` while `command_display`
returned a command.

### 4. `command_display` = shared map for Normal, stash for detail

```rust
pub(crate) fn command_display(
    &self,
    mount_point: &MountPoint,
    ups_config: Option<&Ups>,
) -> Option<String> {
    let request = match self.mode {
        BrowseMode::Normal => self.selection_request(Some(mount_point), ups_config)?,
        // Detail modes render the request actually dispatched into the
        // drill-in, so the footer cannot drift from what ran. This is what
        // makes the subvolume-show footer correct.
        _ => self.last_detail_request.clone()?,
    };
    Some(request.to_argv().to_shell_string())
}
```

`to_argv()` / `to_shell_string()` are unchanged (`cli/src/cmd.rs:446`, `:380`);
view callsite (`cli/src/tui/browse/view.rs:14-28`) is unchanged.

Net effect: ~180 lines of two parallel matches collapse to one ~35-line map
plus two thin wrappers. The detail-mode guards in `command_display` are gone --
the stash subsumes all three detail modes uniformly, which is what fixes subvol.

## Critical files

- `cli/src/tui/browse/state.rs` -- all production changes (field, `dispatch`,
  new `selection_request`, rewritten `current_request` and `command_display`)
  and new tests.
- `cli/src/tui/browse/snapshots/snapshot_browse_subvolume_detail.snap` -- the
  one existing snapshot the refactor changes. Line 19 currently asserts the
  stale footer `$ btrfs subvolume list ''`; once the subvol-detail footer
  renders the dispatched request, this line must be regenerated to
  `$ btrfs subvolume show /mnt/storage/data`. (The list footer shows `''`
  because it renders the empty `model.mount_point` from `Model::new_demo`,
  `cli/src/tui/model.rs:424`; the show command instead uses `pool.mount_point`
  = `/mnt/storage` from `sample_pool()`, `cli/src/tui/demo.rs:128`, joined with
  the `data` subvol path.)

No design-doc / ADR changes: the footer is a read-only TUI cosmetic not
governed by any decision doc (e.g. 022 covers the dry-run/preview model for
*mutating* commands; Browse is read-only). A repo-wide grep found no doc
references to `command_display`.

## Tests

`command_display` has no coverage today. Add behavioral unit tests asserting
the footer string (the `$`-less command) per the existing test patterns
(`BrowseState::default()`, navigate via `state.focus = ...; state.select_next()`,
helpers `pool()` / `ups()` / `disk_inventory()` / `load_current_for_test()`
around `:1404-1456`). Assert literal command strings (pin them from the actual
`to_argv()` output during implementation):

1. **Normal btrfs** -- default selection -> footer is
   `btrfs filesystem usage /mnt/storage`. Pins the shared-map path.
2. **Smartctl picker preview** -- navigate to a smartctl picker selection and
   `load_current` with a populated `disk_inventory()` -> footer is the
   per-device detail command for the selected device. Regression guard for the
   live preview the naive stash-only approach would have broken.
3. **NUT snapshot source** -- navigate to NUT Status with `Some(&ups())` ->
   footer is `upsc <name>`. Pins the intentional snapshot-source behavior.
4. **Subvol detail (the fix)** -- navigate to subvolumes, feed a subvol list
   via `command_finished`, `enter()` to drill in, then assert footer is
   `btrfs subvolume show /mnt/storage/<path>` (today it would be the list
   command). Regression guard for the bug fix.

Each test gets the three-section `//` preamble (Intent / Why it exists /
Scenario) per repo Test Conventions, with the Scenario for #4 naming the
subvol-detail footer mismatch as the concrete incident.

## Verification

- `just test-rust` -- runs the new unit tests plus the existing browse-state
  suite (the `enter`/`reload_detail` effect tests must still pass; the stash is
  additive and does not change the emitted `BrowseRunCommand` effect).
- Regenerate the one affected snapshot. `snapshot_browse_subvolume_detail`
  (`cli/src/tui/browse/view.rs:575`) **will** change its footer from
  `$ btrfs subvolume list ''` to `$ btrfs subvolume show /mnt/storage/data` --
  a known, required fixture update, not a conditional one. `just test-rust`
  fails until `snapshot_browse_subvolume_detail.snap` is accepted
  (`cargo insta accept`, or hand-edit line 19). Confirm no *other* browse
  snapshot moved (none is expected to).
- No VM tests required: this is localized TUI-logic with no systemd / mount /
  pool-lock blast radius (per the "Test scope" guidance, focused rust tests
  only).
