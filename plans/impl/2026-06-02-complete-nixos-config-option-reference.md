# Plan: complete the `braid.*` option reference in `nixos-configuration.md`

## Context

`docs/guides/nixos-configuration.md:5` bills itself as the "Complete reference
for the braid NixOS module options," but the entire `braid.ups.*` option group
is missing. The only NUT/UPS mentions on the page are the pinned-package row
(`:71`) and the toolchain bullet (`:46`). UPS is a featured, opt-in capability
(`README.md:47`, dedicated `docs/guides/ups.md`, ADR `020-ups-integration.md`),
so its absence from the "complete" reference is a real gap: a reader configuring
UPS cannot learn `enable`, the `name`/`driver`/`port` defaults, or that `name`
feeds the CLI via `config.json`.

The four options are declared in `modules/braid/ups.nix:25-57`
(`enable` default `false`, `name` `"ups"`, `driver` `"usbhid-ups"`, `port`
`"auto"`). Only `name` is serialized to `/etc/braid/config.json`
(`modules/braid/cli.nix:32-34`, gated on `cfg.ups.enable`); `driver` and `port`
flow only into nixpkgs `power.ups`.

UPS is the largest gap but not the only one. A full inventory of every
`lib.mkOption` / `lib.mkEnableOption` / `lib.mkPackageOption` in
`modules/braid/*.nix` against the page surfaces two more, which keep the
"complete reference" claim from holding even after the UPS section lands:

- `braid.lockSystemdStopDeadlineSecs` (`modules/braid/options.nix:42-49`; type
  `ints.positive`, default `270`) is a top-level option entirely absent from the
  Core table.
- The full-config example's package-override comments (`:174-177`) list only
  `cryptsetup`/`btrfsProgs`/`utilLinux`, omitting `nut`/`smartmontools`/`ethtool`
  -- even though the Tool overrides table (`:64-73`) already documents all six.

The inventory found no other gaps (the complete declared set is enumerated in
Verification), so closing these alongside UPS makes the claim true rather than
aspirational.

Intended outcome: the reference page documents UPS exactly the way it already
documents the other opt-in features (`autoUnlock`, `autoSuspend`, `fanControl`)
-- an option table under `## Module options`, a block in the full-config
example, and a Related link -- and the two non-UPS gaps above are closed in the
same pass, so the "complete reference" claim holds.

## Scope

Documentation-only. Single file: **`docs/guides/nixos-configuration.md`**.
No code, module, or test changes. Four additive edits (UPS section, UPS
full-config block, Related link, and the non-UPS inventory fixes).

### Why this placement is canonical (not a duplication)

- `docs/guides/ups.md:37` states defaults in *prose* ("Defaults: `name = "ups"`,
  `driver = "usbhid-ups"`, `port = "auto"`") and has **no** option table and
  **no** mention that `name` is written to `config.json`. The new table is the
  canonical option reference, not a copy.
- Established split (verified): each opt-in feature has a workflow guide *and* a
  reference table in `nixos-configuration.md`. `autoSuspend` (`:122-130`) and
  `autoUnlock` (`:107-120`) follow this exactly. UPS is the lone omission.

## Edits

### 1. New `### UPS` subsection under `## Module options`

Append after the **Fan control** subsection (after `:160`, before
`## Full config example` at `:162`). Keeps the opt-in features grouped and the
diff minimal; mirror the same trailing position in the full-config example
(edit 2) so the two sections stay parallel.

```markdown
### UPS

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `braid.ups.enable` | bool | `false` | Enable UPS support via NUT (single-host standalone) |
| `braid.ups.name` | string | `"ups"` | UPS identifier for `upsd`/`upsc` |
| `braid.ups.driver` | string | `"usbhid-ups"` | NUT driver; the USB default covers most home-NAS UPSes |
| `braid.ups.port` | string | `"auto"` | Driver port; `auto` finds the first matching USB UPS |

When enabled, NUT triggers an orderly poweroff on low battery (unwinding `braid-online.service` -> `braid lock` -> unmount) and pool-mutating commands (`add`/`remove`/`remove-missing`/`replace`) refuse to start while the UPS is on battery. Only `name` is written to `/etc/braid/config.json`, so `braid ups status` and the TUI know which UPS to query; `driver` and `port` configure the NUT driver only. Non-USB drivers (`apcsmart`, `snmp-ups`) are an escape hatch and not first-class.

See [UPS](ups.md) for the setup workflow and live status.
```

Note: the `name`-vs-`driver`/`port` distinction (only `name` reaches
`config.json`) is sourced from `cli.nix:32-34` and ADR `020` and is the
value-adding correction beyond the raw finding -- it prevents a reader from
assuming `driver`/`port` are CLI-visible.

### 2. `ups` block in the full-config example

Append inside the `braid = { ... }` block after the `fanControl` block
(after `:216`, before the closing `};` at `:217`):

```nix
  ups = {
    enable = false;        # default; opt-in
    name = "ups";          # default
    driver = "usbhid-ups"; # default
    port = "auto";         # default
  };
```

### 3. Related-list entry

Add to the Related list (`:220-227`), after the Power management entry to group
the power topics:

```markdown
- [UPS](ups.md) -- NUT-backed orderly poweroff, preflight safety, live status
```

(Description phrasing matches `README.md:193`.)

### 4. Close the two non-UPS inventory gaps

So the "complete reference" claim is true after this pass, not just for UPS.

**4a. Add `braid.lockSystemdStopDeadlineSecs` to the Core table** (`:57-62`).
Append this row (type label `positive int` matches how the page renders other
`ints.positive` options, e.g. `autoUnlock.timeoutSec` at `:113`):

```markdown
| `braid.lockSystemdStopDeadlineSecs` | positive int | `270` | Seconds to wait for the pool lock during `braid-online.service` ExecStop; must stay below the unit's `TimeoutStopSec` |
```

**4b. Add `lockSystemdStopDeadlineSecs` to the full-config example** top-level
scalars, right after the `poolAccessGroup` line (`:172`):

```nix
  lockSystemdStopDeadlineSecs = 270;  # default; must stay below braid-online TimeoutStopSec
```

**4c. Complete the package-override comments** in the full-config example
(`:174-177`). Append the three missing entries after `# packages.utilLinux = pkgs.util-linux;`:

```nix
  # packages.nut = pkgs.nut;
  # packages.smartmontools = pkgs.smartmontools;
  # packages.ethtool = pkgs.ethtool;
```

## Deliberate non-changes

- **No back-link from `ups.md` to `nixos-configuration.md`.** The two closest
  siblings (`power-management.md`, `auto-unlock.md`) do not link back from their
  Related sections; only the table-less `fan-control.md` does. Adding one would
  diverge from precedent. Leave `ups.md` as is.
- **Do not add UPS to "What you get for free"** (`:40-51`). That section omits
  the other `enable = false` opt-ins (`autoUnlock`, `autoSuspend`); UPS belongs
  with them, in Module options + the full-config example only.
- **Link style:** bare filename `[UPS](ups.md)` (same dir), matching
  `[Fan control](fan-control.md)` at `:160`. Not `guides/ups.md`.

## Verification

- **Linkcheck:** `nix develop .#docs -c mdbook build docs` -- `mdbook` is not on
  the bare shell PATH; it lives in the `.#docs` devshell (the same one
  `just docs` enters, `justfile:211`). The build runs `mdbook-linkcheck2`
  (configured in `docs/book.toml`), which validates every cross-link and
  confirms the new `[UPS](ups.md)` link resolves. Note `just check-docs` does
  *not* run the build -- it only checks SUMMARY.md parity, escape links, and
  doc-table order -- so it is not a substitute for the linkcheck here.
- **Inventory cross-check** (the claim this plan rests on): re-run
  `rg -n "mkOption|mkEnableOption|mkPackageOption" modules/braid/*.nix` and
  confirm every declared option appears in a Module-options table. After these
  edits the documented set equals the declared set: Core
  (`enable`, `package`, `mountPoint`, `poolAccessGroup`,
  `lockSystemdStopDeadlineSecs`), `packages.*` (all six), `autoUnlock.*`,
  `autoScrub.*`, `monitor.*`, `ups.*`, `autoSuspend.*`, `fanControl.*` -- no
  remaining gaps.
- **Manual read-through:** UPS defaults in the table and full-config block match
  `modules/braid/ups.nix:26,30,40,51`; the `config.json` sentence matches
  `modules/braid/cli.nix:32-34,46`; the preflight command list matches
  `cli/src/preflight.rs:575` and `docs/guides/ups.md:14`;
  `lockSystemdStopDeadlineSecs` default `270` matches
  `modules/braid/options.nix:42-44`.
- **Parallel structure:** the option-table subsection order and the full-config
  block order both end with UPS, consistent with each other.
