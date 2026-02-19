# NixOS Best Practices for btrnas

## Goal

Default to the most NixOS-conventional solution for every design choice.

We deviate only for clear, documented reasons (hardware limits, security constraints, operational risk, or missing platform capability).

## Decision rule

When evaluating an implementation:

1. What is the standard NixOS module-system approach?
2. Can we implement that directly with typed options + declarative config?
3. If not, what constraint forces deviation?
4. Is the deviation temporary or permanent?
5. How do we test and document it?

If a deviation is chosen, it must include:

- Why the conventional approach was not used
- The tradeoff accepted
- A condition that would let us return to the conventional approach

## Practices list (living)

This list accumulates concrete examples over time.

### 1) Declarative first, imperative second

- NixOS options are the source of truth for system state.
- Runtime tools read exported config (`/etc/btrnas/config.json`) instead of reconstructing intent from mutable live state.
- Scripts perform one-shot operational actions and reject operations that would create config drift (e.g. formatting a disk not declared in `btrnas.disks`).

Example: `btrnas.disks` and `btrnas.mountPoint` are declared in module config and exported to `/etc/btrnas/config.json`. `btrnas-add-disk` reads that file, formats the disk, and adds it to the pool — but refuses to touch any disk not listed in the config.

### 2) Follow standard module conventions

Use `mkEnableOption`, typed `mkOption` with defaults/descriptions, `mkIf`, and `mkDefault`. Let the module system handle merging and evaluation — avoid shell/runtime conditionals for policy that belongs at eval time.

This is baseline NixOS module knowledge, not a btrnas-specific decision. See the [NixOS module system tutorial](https://nix.dev/tutorials/module-system/a-basic-module/index.html) for details.

### 3) Nix→runtime config bridge

The module exports config to `/etc/btrnas/config.json` via `environment.etc`. This is the single channel for Nix config to reach runtime tools.

Rules:
- All CLI tools read `/etc/btrnas/config.json` by default (with `--config` override for rescue/test).
- Future tools use the same file — don't invent new config mechanisms.
- The file is built at `nixos-rebuild` time and is read-only at runtime.

Example: `btrnas-add-disk` reads the disks list and mount point from `config.json` to know which disks are valid targets and where the pool is mounted.

### 4) Validate early with assertions and warnings

- Catch invalid states during evaluation where possible.
- Use `assertions`/`warnings` for user-facing clarity.

### 5) Stable identifiers for persistent storage config

- Prefer `/dev/disk/by-id` or UUID-based references.
- Do not rely on unstable kernel names like `/dev/sdX` in persistent config.

### 6) Resilience by default

- Missing/broken data disks must not block boot.
- Prefer NixOS-native fault-tolerant mount/unlock options (`nofail`, bounded timeouts, degraded mount where appropriate).

### 7) Package operational tools declaratively

- Build scripts with `pkgs.writeShellApplication`.
- Declare all runtime dependencies via `runtimeInputs`.
- Install tools through module config when feature is enabled.

### 8) Test architecture decisions with NixOS VM tests

- Every design policy change should have a regression test.
- Tests should validate behavior, not just command success.

## Deviation log

### Deviation: imperative disk formatting (`btrnas-add-disk`)

- **Conventional approach:** Declarative `fileSystems` / `luks.devices` handles everything at `nixos-rebuild` time.
- **Why not used:** `cryptsetup luksFormat` and `btrfs device add` are destructive one-shot operations that cannot be made idempotent — re-running them would destroy data.
- **Chosen approach:** Config-first workflow with an imperative executor. User declares the disk in `btrnas.disks`, runs `nixos-rebuild switch` (which creates LUKS entries and exports config), then runs `btrnas-add-disk` to format and join the pool.
- **Tradeoffs:** Two-step process (rebuild + run script) instead of a single rebuild. Script must validate against config to prevent drift.
- **Revisit trigger:** If NixOS ever gets a `formatDevice` option type that can safely express one-shot destructive operations.
- **Test coverage:** `btrnas-add-disk` test validates the full flow: config export → LUKS format → btrfs create/add → mount verification.

## Sources used

- NixOS Manual (module system, options, assertions/warnings): https://nixos.org/nixos/manual/index.html
- NixOS Manual (filesystem and boot/initrd/LUKS configuration patterns): https://nixos.org/manual/nixos/unstable/
- Nixpkgs Manual (`pkgs.writeShellApplication`, packaging shell tools): https://nixos.org/nixpkgs/manual
- nix.dev (module-system tutorial and best-practices guidance): https://nix.dev/tutorials/module-system/a-basic-module/index.html
- nix.dev best practices: https://nix.dev/guides/best-practices.html
