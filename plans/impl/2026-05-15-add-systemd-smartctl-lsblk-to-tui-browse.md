# Add Systemd, Smartctl, and Lsblk to TUI Browse

## Context

The TUI Browse tab currently exposes two read-only programs -- Btrfs and
NUT -- behind a Program / Command / Subview / Content layout
(`cli/src/tui/browse/state.rs`). The most common questions a braid NAS
admin asks have no Browse answer today:

- "Is the daemon stack healthy?" -- needs `systemctl` views.
- "Are my drives healthy?" -- needs `smartctl` views.
- "What block devices are present and how are they layered?" -- needs
  `lsblk` views.

This change adds three new BrowseProgram entries with curated, ordered
read-only command lists. Per-device `smartctl` and per-unit `systemctl
status`/`show` reuse the existing subvolume drill-in pattern
(`BrowseMode::SubvolDetail`) so the UX shape stays uniform.

Assumes the currently-staged tree (the Browse-folded-into-tui refactor)
is committed before implementation starts; this plan layers on top of
that landing state.

## Programs and commands (ordered)

New `BrowseProgram` variants, appended after `Nut` so existing muscle
memory and the `command_rows_append_new_read_only_groups` test
invariant hold:

```
Btrfs  |  NUT  |  Systemd  |  SMART  |  lsblk
```

### Systemd (`Systemd`, label `"Systemd"`)

Six commands. Two have drill-in pickers, four are raw pass-through.

| # | Command  | Underlying invocation                                                                                    | Mode               |
|---|----------|----------------------------------------------------------------------------------------------------------|--------------------|
| 1 | Status   | `systemctl list-units --output=json --all 'braid-*' 'hddfancontrol-braid.service'`                       | picker -> drill-in to `systemctl status <unit> --no-pager` |
| 2 | Show     | (same picker source)                                                                                     | picker -> drill-in to `systemctl show <unit> --no-pager`   |
| 3 | Braid    | `systemctl list-units --all --no-pager 'braid-*' 'hddfancontrol-braid.service'`                          | raw                |
| 4 | Failed   | `systemctl list-units --failed --all --no-pager`                                                         | raw                |
| 5 | Timers   | `systemctl list-timers --all --no-pager`                                                                 | raw                |
| 6 | Mounts   | `systemctl list-units --type=mount,automount --all --no-pager`                                           | raw                |

Rationale: Status answers the "is this broken?" question per unit and is
the most-reached-for view. Show is the same picker with a heavier
drill-in target (`systemctl show` key=value dump). Braid is the
human-friendly summary table. Failed/Timers/Mounts are universal admin
shortcuts. `--no-pager` is needed because systemctl pages by default
when stdout is a tty; harmless when invoked via `Command`.

The braid daemon stack includes one unit that does not match the
`braid-*` prefix: `hddfancontrol-braid.service`
(`modules/braid/fan-control.nix:157`), the chassis-safety fan loop
that the Data tab also treats as a first-class liveness source
(`cli/src/tui/probe.rs:436`). The Status picker and the Braid raw view
both pass it as an explicit second positional pattern so a healthy
fan daemon is visible in the same place as the rest of the stack.
systemctl accepts multiple positional patterns and unions their
matches.

The picker source uses `--output=json` (not text columns) because the
text table is not fixed-width: systemctl hides the JOB column only
when there are no pending jobs
(`reference/systemd/src/systemctl/systemctl-list-units.c:209`) and the
table goes through `output_table` which has a first-class JSON path
(`reference/systemd/src/systemctl/systemctl-util.c:919`). JSON gives
stable fields (`unit`, `load`, `active`, `sub`, `description`) and
eliminates the need for whitespace heuristics. This introduces a
UI-only systemctl parse boundary; document it in `docs/principles.md`
and `docs/decisions/010-toolchain-pinning.md` so the existing "systemd
output is never parsed" rule remains coherent. See the doc-update
section below for the exact edits.

No empty state. systemctl works regardless of pool state.

### SMART (`Smartctl`, label `"SMART"`)

Six commands. Five have drill-in device pickers.

| # | Command       | Underlying invocation             | Mode                                       |
|---|---------------|-----------------------------------|--------------------------------------------|
| 1 | Scan          | `smartctl --scan`                 | raw, no device                             |
| 2 | Health        | (device picker)                   | picker -> `smartctl -H <by-id>`            |
| 3 | Info          | (device picker)                   | picker -> `smartctl -i <by-id>`            |
| 4 | Attributes    | (device picker)                   | picker -> `smartctl -A <by-id>`            |
| 5 | Self-test Log | (device picker)                   | picker -> `smartctl -l selftest <by-id>`   |
| 6 | Error Log     | (device picker)                   | picker -> `smartctl -l error <by-id>`      |

Picker source: `Model.disk_by_id: HashMap<String, String>`
(`cli/src/tui/model.rs:278`). Disk name shown to user; `/dev/disk/by-id/...`
path passed to smartctl for stable identity across reboots.

Rationale: Scan first because it's the only device-less command and
answers "what does the kernel see?" without needing braid state. Health
and Attributes are the two SMART views a NAS admin reaches for daily.

Empty state: commands 2-6 with empty `disk_by_id` -> `"no disks known
to braid -- run `braid discover` or add disks first"`. Scan runs
unconditionally.

### lsblk (`Lsblk`, label `"lsblk"`)

Five raw commands, no drill-in.

| # | Command      | Underlying invocation |
|---|--------------|-----------------------|
| 1 | Tree         | `lsblk`               |
| 2 | Filesystems  | `lsblk -f`            |
| 3 | Disks        | `lsblk -d`            |
| 4 | All Columns  | `lsblk -O`            |
| 5 | SCSI         | `lsblk -S`            |

Rationale: Tree (default) is the parent/child mental model. `-f`
confirms LUKS + btrfs layering -- the single most common braid sanity
check. `-d` strips noise. `-O` for power users; `-S` for SCSI/SAS edge
case. lsblk has no `--no-pager`; it doesn't paginate when stdout is not
a tty.

No empty state. lsblk works regardless of pool state.

## Code changes

### 1. `cli/src/cmd.rs` -- new `CmdRequest` variants

Add 14 new variants alongside existing patterns. Follow
`SmartctlHealthJson { device }` for the smartctl variants (line 173,
787), `SystemctlIsActive { unit }` for the systemctl variants (line
278, 964), and `LsblkJson` for the lsblk variants (line 22, 378):

```rust
// Systemd
SystemctlListUnitsBraid,        // raw (Braid command)
SystemctlListUnitsBraidJson,    // picker source (--output=json)
SystemctlListUnitsFailed,
SystemctlListTimers,
SystemctlListMounts,
SystemctlStatusUnit { unit: String },
SystemctlShowUnit { unit: String },

// Smartctl (raw human text, distinct from existing SmartctlHealthJson)
SmartctlScan,
SmartctlHealth { device: String },
SmartctlInfo { device: String },
SmartctlAttributes { device: String },
SmartctlSelftestLog { device: String },
SmartctlErrorLog { device: String },

// Lsblk (raw human text, distinct from existing LsblkJson)
LsblkTree,
LsblkFilesystems,
LsblkDisks,
LsblkAllColumns,
LsblkScsi,
```

Each gets a `to_argv()` arm returning `CmdArgs { program, args }` per
the existing pattern. `--no-pager` is appended only to systemctl args.

### 2. `cli/src/tui/browse/state.rs` -- Browse plumbing

**`BrowseProgram`**: add `Systemd`, `Smartctl`, `Lsblk`. Extend `ALL`
to `[Btrfs, Nut, Systemd, Smartctl, Lsblk]`. Add labels.

**`BrowseCommand`**: add 17 new variants (Systemd: 7 incl. picker pair;
SMART: 6; lsblk: 5). Add per-program command arrays `SYSTEMD: [..]`,
`SMART: [..]`, `LSBLK: [..]`. Extend `commands()` and add per-program
remembered selection fields (`systemd_command`, `smartctl_command`,
`lsblk_command`) mirroring `btrfs_command` / `nut_command`.

**`BrowseSelection`**: add a variant per new BrowseCommand. None have
subviews (no fourth column).

**`BrowseMode`**: generalize the drill-in mode set.

```rust
enum BrowseMode {
    Normal,
    SubvolDetail,
    SmartctlDeviceDetail,
    SystemdUnitDetail,
}
```

**Picker state**: add owned-row state for each picker, paralleling
`subvolumes: Vec<BtrfsSubvolume>` + `subvol_selected: usize`:

```rust
smartctl_devices: Vec<(String, String)>,  // (friendly, by-id path)
smartctl_selected: usize,
systemd_units: Vec<SystemdUnitRow>,        // parsed list-units rows
systemd_unit_selected: usize,
```

Plus pre-drill-in output snapshots (`smartctl_picker_output`,
`systemd_picker_output`) so `back()` restores the picker exactly the
way `subvol_list_output` does.

**`load_current` signature**: extend with disk inventory borrow.

```rust
pub(crate) fn load_current(
    &mut self,
    pool: &PoolStatus,
    ups_config: Option<&Ups>,
    disks: &DiskInventory<'_>,
) -> Option<Effect>;

pub struct DiskInventory<'a> {
    pub by_id: &'a HashMap<String, String>,  // friendly_name -> by-id path
}
```

**Smartctl picker population is synchronous, no command.** In
`load_current`, when the active selection is a Smartctl per-device
command (Health/Info/Attributes/Selftest/Errors) in Normal mode:

1. If `disks.by_id.is_empty()`, install `BrowseEmptyState::NoDisksKnown`
   and return `None`.
2. Otherwise, populate `self.smartctl_devices` from `disks.by_id`
   (sorted for stable rendering), clamp `smartctl_selected`, leave
   `self.output` empty (or render a one-line "press Enter for SMART
   data" hint), and return `None`.

The picker has no associated `CmdRequest` and never enters
`command_finished` while in Normal mode. Smartctl Scan still runs an
async command (`SmartctlScan`) like any other raw entry.

**Systemd picker population is async.** In `load_current`, when the
active selection is Systemd Status or Show in Normal mode, dispatch
`SystemctlListUnitsBraidJson` as the picker source.
`command_finished` parses the JSON array (fields per row: `unit`,
`load`, `active`, `sub`, `description`) into
`self.systemd_units: Vec<SystemdUnitRow>` and clamps
`systemd_unit_selected`. Parser failure falls back to raw-text
rendering with an empty `systemd_units` list (Enter becomes a no-op),
mirroring `parse_btrfs_subvolume_list`'s permissive contract.

**`enter()`**: extend the dispatch to also handle the smartctl and
systemd pickers. For smartctl, snapshot `output` (typically empty
hint) into `smartctl_picker_output`, set
`BrowseMode::SmartctlDeviceDetail`, and dispatch
`SmartctlHealth { device: ... }` etc. with the by-id path of the
selected device. For systemd, snapshot the raw JSON output into
`systemd_picker_output`, set `BrowseMode::SystemdUnitDetail`, and
dispatch `SystemctlStatusUnit { unit: ... }` or `SystemctlShowUnit
{ unit: ... }` depending on whether Status or Show is the active
BrowseSelection.

**`back()`**: extend to restore picker output for the new detail
modes. For smartctl, also re-derive `smartctl_devices` from the latest
`DiskInventory` if needed (handled cleanly by re-entering
`load_current` semantics).

**`reload_detail`**: extend to re-dispatch the per-device or per-unit
command for the active detail mode.

**`command_finished`**: extend with one new parsing branch for the
systemd picker (`SystemctlListUnitsBraidJson` -> populate
`systemd_units` via `parse_systemctl_list_units_json`). All other new
Browse selections (smartctl Scan, lsblk *, systemd raw views, detail
mode for smartctl/systemd) follow the existing raw-text path.

**`is_subvolume_list` -> generalized `is_picker_list()`** (or add
parallel `is_smartctl_picker()`, `is_systemd_picker()` query methods)
so the view can switch between three table renderers without ballooning
match arms.

**Empty-state gating**: extend
`BrowseEmptyState` with `NoDisksKnown` and add a per-selection check
mirroring `requires_ups_name`. systemd commands have no empty state.

### 3. `cli/src/tui/browse/view.rs` -- new renderers

Add two table renderers modeled on `render_subvolume_table` (line 155):

- `render_smartctl_device_table` -- columns: marker, Name, ByIdPath.
- `render_systemd_unit_table` -- columns: marker, Unit, Load, Active,
  Sub, Description.

In `render_content`, branch to the new tables when `browse.is_smartctl_picker()`
or `browse.is_systemd_picker()` is true and the respective list is
non-empty. Detail mode (`SmartctlDeviceDetail`,
`SystemdUnitDetail`) falls through to the default raw-line renderer.

### 4. `cli/src/tui/app.rs` -- pass disks into `load_current`

Update the call sites at `cli/src/tui/app.rs:328` and the
`browse_load_if_active` helper to construct `DiskInventory { by_id:
&model.disk_by_id }` and pass it down. Same for `reload_detail` and
`enter` if they need disk inventory (enter does -- the smartctl picker
drill-in builds the CmdRequest from the selected device's by-id path).

### 5. Both wrappers -- expose `smartctl` deterministically

braid has two wrappers and the VM test path goes through the second,
not the first:

1. `modules/braid/wrapper.nix` -- the shell-script wrapper installed
   into `environment.systemPackages` for end users
   (`modules/braid/cli.nix:41`). Currently exposes
   `[cryptsetup, btrfsProgs, utilLinux, nut, pkgs.systemd]`.
2. `flake.nix` `braid` derivation -- `pkgs.runCommand` +
   `makeWrapper --prefix PATH` (`flake.nix:48-52`). Currently exposes
   `[pkgs.cryptsetup, pkgs.btrfs-progs, pkgs.util-linux]`. This is the
   wrapper that `tests/cli/braid-tui-browse.nix:56` invokes via
   `${braid}/bin/braid tui` in the `braid-tui-canary.service`. A
   systemd-managed service does not inherit the user shell's
   `/run/current-system/sw/bin`-style PATH dependably, so binaries the
   wrapper does not pin are effectively unavailable to this test.

**Edit `modules/braid/options.nix`**: smartmontools is already a
parsed tool (`cli/src/parse/smartctl.rs:76`, invoked by
`SmartctlHealthJson` at `cli/src/tui/probe.rs:251`) but is not yet a
module-controlled package. Add it to `braid.packages.*` so operators
can override the version the way they can for the other parsed tools:

```nix
packages = {
  cryptsetup    = lib.mkPackageOption pkgs "cryptsetup"    { };
  btrfsProgs    = lib.mkPackageOption pkgs "btrfs-progs"   { };
  utilLinux     = lib.mkPackageOption pkgs "util-linux"    { };
  nut           = lib.mkPackageOption pkgs "nut"           { };
  smartmontools = lib.mkPackageOption pkgs "smartmontools" { };  # NEW
};
```

**Edit `modules/braid/wrapper.nix`**: use the new module-controlled
package, not `pkgs.smartmontools`:

```nix
toolPackages =
  (with cfg.packages; [
    cryptsetup
    btrfsProgs
    utilLinux
    nut
    smartmontools                    # NEW
  ])
  ++ [ pkgs.systemd ];
```

**Edit `flake.nix`** in two places.

First, the local braid-wrapper toolPath used by `nix run` and VM
tests (`flake.nix:43-47`). Add `pkgs.systemd` and `pkgs.smartmontools`
so the VM-test wrapper path can resolve `systemctl` and `smartctl`.
flake.nix has no access to `cfg.packages.*`, so it uses the flake's
pinned nixpkgs directly -- matches the existing treatment of
`pkgs.cryptsetup`, `pkgs.btrfs-progs`, and `pkgs.util-linux`:

```nix
toolPath = pkgs.lib.makeBinPath [
  pkgs.cryptsetup
  pkgs.btrfs-progs
  pkgs.util-linux
  pkgs.systemd
  pkgs.smartmontools
];
```

Second, the `nixosModules.default` block at `flake.nix:840-847`. The
flake currently pins `cryptsetup`/`btrfsProgs`/`utilLinux` from
`braidPkgs` (`flake.nix:842-846`). Add a pinned `smartmontools`
default so consumers of the flake module get the parser-pinned
version rather than their own `pkgs.smartmontools`:

```nix
config.braid = {
  package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.braid-cli-unwrapped;
  packages = {
    cryptsetup    = lib.mkDefault braidPkgs.cryptsetup;
    btrfsProgs    = lib.mkDefault braidPkgs.btrfs-progs;
    utilLinux     = lib.mkDefault braidPkgs.util-linux;
    smartmontools = lib.mkDefault braidPkgs.smartmontools;   # NEW
  };
};
```

`lsblk` is already on PATH via `pkgs.util-linux` in both wrappers; no
edit needed for it.

### 7. Doc updates -- smartmontools parser contract and systemd JSON exception

Two doc-truth corrections fall out of this change. Both belong in the
same change set so future fixture/toolchain work is not given
conflicting instructions.

**A. smartmontools is parser-critical but currently absent from the
package contract.** `cli/src/parse/smartctl.rs:76` parses
`smartctl --json` output and `AGENTS.md:207` already lists smartmontools
among the parsed tools, yet `modules/braid/options.nix:20-25`,
`docs/principles.md:59`, the toolchain table at
`docs/decisions/010-toolchain-pinning.md:32-39`, and the flake module
defaults at `flake.nix:840-847` all omit it. The options.nix and
flake.nix edits above close the package side. The doc side needs:

- `docs/principles.md` (Principle 10, line 59): add `smartmontools`
  to the parser-critical tool list at the start of the paragraph
  (`btrfs-progs, cryptsetup, util-linux, NUT` -> `btrfs-progs,
  cryptsetup, util-linux, NUT, smartmontools`). Keep the systemd
  UI-only exception from part B below in the same paragraph.
- `docs/decisions/010-toolchain-pinning.md`: in the Context (line 7)
  add smartmontools to the parser-critical tool list. In the table at
  lines 32-39, add a new row:
  `| smartmontools | Yes | Yes (braid.packages.smartmontools) | smartctl --json output parsed by parse_smartctl |`.
- `AGENTS.md`: in the parser-critical tool-versions sentence at line
  216, add smartmontools to the list (`btrfs-progs, cryptsetup,
  util-linux, nut` -> `btrfs-progs, cryptsetup, util-linux, nut,
  smartmontools`) and add smartmontools to the
  `braid.packages.{...}` enumeration in the same sentence.

**B. The systemd JSON list-units parser is the first systemctl output
parser in braid and contradicts existing authority that systemd is
"not part of braid's parser contract".** Resolve in the same docs:

- `docs/principles.md` (Principle 10, line 59): qualify the "Generic
  helpers (coreutils, systemd) ... not part of braid's parser
  contract" sentence with one additional clause noting that the TUI
  Browse Systemd picker parses `systemctl list-units --output=json`
  as a tolerant UI-only exception (fallback to raw text on parse
  failure; not parser-critical; systemd remains non-pinned).
- `docs/decisions/010-toolchain-pinning.md`: in the Context (line 7),
  the Classification guideline (line 24+), and the toolchain table
  row for systemd (line 39), add the same qualifier. Keep systemd in
  the "Use system pkgs" / "No - system pkgs" classification; the
  exception is a single UI-only JSON parser, not a parsing contract.

No new ADR is needed -- amending the existing Active ADR captures both
the new parser-critical row and the single UI-only exception in one
place. If the systemd exception ever grows beyond the list-units
picker, that triggers a follow-up ADR.

### 6. Parser for `systemctl list-units --output=json`

Tiny parser, file `cli/src/parse/systemctl_list_units.rs`:

```rust
pub struct SystemdUnitRow {
    pub unit: String,
    pub load: String,
    pub active: String,
    pub sub: String,
    pub description: String,
}

pub fn parse_systemctl_list_units_json(raw: &RawCommandOutput)
    -> Result<Vec<SystemdUnitRow>, ParseError>;
```

`--output=json` emits a JSON array of objects with stable fields
(`unit`, `load`, `active`, `sub`, `description`, plus `job` when
present). Deserialize via `serde_json` into a `Vec<SystemdUnitRow>`.
The text table is NOT parsed because systemctl hides the JOB column
only when there are no pending jobs
(`reference/systemd/src/systemctl/systemctl-list-units.c:209`), so
whitespace-column heuristics would silently misalign on a host with a
pending job. JSON is the contract systemctl itself routes the table
through (`reference/systemd/src/systemctl/systemctl-util.c:919`).

Add a fixture-backed unit test in the parser module:
`cli/tests/fixtures/systemctl-list-units-braid.json` (a small captured
JSON array covering the braid-* unit shape). No `nixos-25.11/`
versioned fixture dir and no `just test-parsers` canary entry, because
the JSON contract is owned by systemd itself and is far more stable
than the text table; this stays a TUI-only parser by design. Parser
failure falls back to raw text rendering and disables Enter (mirrors
`parse_btrfs_subvolume_list` permissive handling at `state.rs:524`).

This is the first systemctl output parser in braid. The
authoritative project rules currently say systemd output is never
parsed and systemd is outside braid's parser contract
(`docs/principles.md:59`,
`docs/decisions/010-toolchain-pinning.md:7,11,17`, and the toolchain
table row for systemd at `010-toolchain-pinning.md:39`). The
doc-update section below captures the exact edits needed so future
fixture/toolchain work is not given contradictory instructions.

## Tests

Mirror the existing test inventory; add nothing the existing rubric
doesn't justify.

### Rust unit tests (`cli/src/cmd.rs`) -- `to_argv` argv contracts

The selection-to-request tests below only assert that the correct
`CmdRequest` variant is chosen; they would still pass if e.g.
`SmartctlErrorLog` accidentally invoked `-l selftest`, or
`SystemctlStatusUnit` omitted `--no-pager`, or an lsblk flag drifted.
The argv shape is the user-facing contract for these commands (it is
shown verbatim in the Browse footer via `command_display`), so pin it
explicitly.

Add a table-driven test in `cli/src/cmd.rs` (alongside any existing
`to_argv` coverage; if no such test module exists yet, add one) that
asserts the exact `(program, args)` shape produced by `to_argv()` for
every new variant introduced by this plan: the seven Systemd variants
(including `SystemctlStatusUnit` / `SystemctlShowUnit` with a sample
unit name), the six Smartctl variants (Scan + five per-device), and
the five lsblk variants. One row per variant -- a regression in any
single arm fails its own assertion.

Pin the daemon-stack coverage explicitly: the argv test rows for
`SystemctlListUnitsBraid` and `SystemctlListUnitsBraidJson` must
assert that BOTH `braid-*` and `hddfancontrol-braid.service` appear
as positional patterns, in that order, after the flags. A regression
that drops the fan-control unit (e.g. someone "cleans up" to just
`braid-*`) fails the test immediately rather than silently dimming
the Browse picker.

### Rust unit tests (`cli/src/tui/browse/state.rs` test module)

Extend the four existing test categories:

1. **Command-row ordering** -- extend
   `command_rows_append_new_read_only_groups` to add asserts for
   Systemd, SMART, lsblk command lists in the exact order specified
   above. New top-level row order on Program column:
   `[Btrfs, NUT, Systemd, SMART, lsblk]`.
2. **Selection-to-request mapping** -- extend
   `new_browse_selections_map_to_expected_requests` to cover all 17
   new commands. For per-device/per-unit drill-ins, assert Normal-mode
   behavior matches the picker contract: Smartctl per-device commands
   return `None` from `load_current` and populate
   `smartctl_devices` synchronously from the supplied `DiskInventory`;
   Systemd Status/Show dispatch `SystemctlListUnitsBraidJson` as the
   picker source command.
3. **Empty state** -- new test
   `smartctl_per_device_without_disks_sets_empty_state`: selecting
   Smartctl Health with `DiskInventory { by_id: empty }` sets
   `BrowseEmptyState::NoDisksKnown` and returns no effect. New test
   `smartctl_scan_runs_without_disks`: Scan dispatches `SmartctlScan`
   regardless of disk inventory.
4. **Drill-in** -- new tests
   `enter_in_smartctl_device_row_drills_in` and
   `enter_in_systemd_unit_row_drills_in` mirror
   `enter_in_subvolume_row_drills_in`: select a row, press Enter,
   assert dispatched effect carries the correct device/unit. New test
   `esc_pops_back_from_smartctl_detail` mirrors `esc_pops_back`.

### Snapshot tests (`cli/src/tui/browse/view.rs`)

Add seven snapshots in the existing snapshot test module (one per new
visible state):

- `snapshot_browse_systemd_status_picker`
- `snapshot_browse_systemd_status_detail`
- `snapshot_browse_systemd_failed`
- `snapshot_browse_smartctl_scan`
- `snapshot_browse_smartctl_health_picker`
- `snapshot_browse_smartctl_health_detail`
- `snapshot_browse_lsblk_filesystems`

Use `Model::new_demo` with `sample_disk_names()` and feed
`command_finished` synthetic output. The detail snapshots set mode
and call `enter` first, then feed the per-target output (mirrors
`snapshot_browse_subvolume_detail` at `view.rs:462`).

`Model::new_demo` sets `disk_by_id: HashMap::new()`
(`cli/src/tui/model.rs:388`), so the SMART picker and SMART detail
snapshots must seed `model.disk_by_id` explicitly before calling
`load_current` / `enter` -- otherwise they exercise
`BrowseEmptyState::NoDisksKnown` instead of the intended picker /
detail behavior. Use the same disk-name -> by-id mapping the existing
Btrfs snapshots imply (e.g. `disk1 -> /dev/disk/by-id/<sample>`); a
single shared helper in the snapshot test module is fine.

### VM test

Extend `tests/cli/braid-tui-browse.py` with three new sequences after
the existing subvolume drill-in (the file already drives Btrfs end to
end; piggyback on the same VM rather than a new check):

1. Navigate to **Systemd > Status**, assert the picker shows
   `braid-online.service` (or another known braid unit guaranteed to
   exist in the VM), press Enter, assert detail output contains
   `Loaded:`.
2. Navigate to **SMART > Health**, assert the picker shows the
   `disk1`/`disk2` rows that the fixture configures
   (`tests/cli/braid-tui-browse.nix:14-17`). Press Enter on a row,
   assert detail output mentions the selected disk's by-id path (or
   the smartctl error message that names the device path). Do NOT
   assert on SMART > Scan: smartctl's default Linux scan walks
   `/dev/hd*`, `/dev/sd*`, `/dev/nvme*`
   (`reference/smartmontools/smartmontools/os_linux.cpp:3108-3137`)
   and skips the `/dev/vd*` virtio devices NixOS VM tests create, so
   `--scan` legitimately returns empty in this VM and a `/dev/` assert
   would be flaky.
3. Navigate to **lsblk > Filesystems**, assert output contains `btrfs`
   somewhere.

No new `.nix` test wrapper or `flake.nix` checks entry needed -- reuses
the existing `braid-tui-browse` check.

### Parser canary

No fixture or `just test-parsers` change. New Browse commands are raw
pass-through except for the systemd list-units picker parser, which is
parser-tolerant (falls back to raw text on parse failure). The picker
parser's correctness is covered by Rust unit tests, not a VM canary.

## Critical files to modify

- `cli/src/cmd.rs` (add 18 CmdRequest variants + to_argv arms)
- `cli/src/tui/browse/state.rs` (BrowseProgram, BrowseCommand,
  BrowseSelection, BrowseMode, picker state, load_current signature,
  enter/back/reload_detail, command_finished branches, empty state)
- `cli/src/tui/browse/view.rs` (two new table renderers, snapshot tests)
- `cli/src/tui/app.rs` (pass DiskInventory through to load_current,
  reload_detail, enter)
- `cli/src/parse/systemctl_list_units.rs` (new parser file)
- `cli/src/parse/mod.rs` (re-export)
- `modules/braid/options.nix` (add `smartmontools` to `braid.packages.*`)
- `modules/braid/wrapper.nix` (use `cfg.packages.smartmontools` in toolPackages)
- `flake.nix` (add `pkgs.systemd` and `pkgs.smartmontools` to `toolPath`)
- `tests/cli/braid-tui-browse.py` (extend with three new sequences)
- `docs/principles.md` (Principle 10: add smartmontools to the parser-critical tool list AND qualify with the UI-only systemd JSON parser exception)
- `docs/decisions/010-toolchain-pinning.md` (add smartmontools row to the toolchain table; amend Context, Classification guideline, and toolchain table row for systemd)
- `AGENTS.md` (line 216: add smartmontools to the parser-critical tool list and `braid.packages.{...}` enumeration)

## Reused functions / types

- `CmdRequest` and `CmdArgs::to_shell_string` -- `cli/src/cmd.rs:21,
  ~309`. Patterns to mirror: `SmartctlHealthJson` (line 173/787),
  `SystemctlIsActive` (line 278/964), `LsblkJson` (line 22/378).
- `BrowseMode::SubvolDetail` and the surrounding `enter` / `back` /
  `reload_detail` flow -- `cli/src/tui/browse/state.rs:460-498`. Direct
  model for both smartctl and systemd drill-in.
- `render_subvolume_table` -- `cli/src/tui/browse/view.rs:155`.
  Pattern for both new picker tables.
- `parse_btrfs_subvolume_list` -- `cli/src/parse/btrfs_subvolume_list.rs`.
  Pattern for the new systemd unit parser (including the
  permissive error fallback).
- `Model.disk_by_id` -- `cli/src/tui/model.rs:278`. Authoritative
  source for stable smartctl device paths.
- `BrowseEmptyState` -- `cli/src/tui/browse/state.rs:227-242`. Pattern
  for the new `NoDisksKnown` state.

## Verification

1. **Rust unit tests** -- `just test-rust`. Confirms command-row
   ordering, selection-to-request mapping, empty-state gating, and
   drill-in semantics for all three new programs.
2. **Snapshot tests** -- `cargo insta review` (inside `cli/`) to
   inspect new view snapshots; commit accepted snapshots.
3. **VM test** -- `just test-vm braid-tui-browse`. Walks the new
   Systemd / SMART / lsblk tabs in a real VM TUI session against real
   systemctl / smartctl / lsblk binaries.
4. **Parser canary** -- `just test-parsers` should pass unchanged.
   Confirms no regression in the existing parser-backed Browse paths.
5. **Manual smoke** -- in a dev VM, run `braid tui`, navigate:
   - Systemd > Status, Enter on a unit, verify detail; Esc back.
   - SMART > Scan, observe scan output; SMART > Health, Enter on a
     disk, verify health line; Esc back.
   - lsblk > Filesystems, verify `crypto_LUKS` and `btrfs` rows.
   - Smartctl > Health with no disks (e.g. fresh install pre-discover)
     shows the `NoDisksKnown` empty state.
