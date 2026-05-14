# Plan: Add read-only Browse commands for Btrfs and NUT

## Summary

Add the first batch of read-only raw-output views to the existing `braid tui`
Browse tab. This expands Btrfs inspection beyond the current
filesystem/device/subvolume/scrub/balance basics, and adds NUT
discovery/client/settable-variable listing without introducing any new
top-level CLI command.

## Key Changes

- Extend Browse command groups (ordering: append new top-level command groups; existing entries keep their position, per ADR 025):
  - `Btrfs > Filesystem`: append `Commit Stats` subview -> `btrfs filesystem commit-stats <mount>`.
  - `Btrfs > Subvolumes`: convert from single command into a subview group with `List`, `Full`, `Snapshots`, `Deleted`, `Default` (in that order; `List` is the default subview so existing Subvolumes drill-in behavior is preserved).
  - `Btrfs > Scrub`: convert from single command into a subview group with `Status`, `Limits` (in that order; `Status` is the default subview).
  - Append `Btrfs > Quota` after existing `Balance`: subviews `Status`, `Qgroups`.
  - Append `Btrfs > Inspect` after `Quota`: subview `Chunks`.
  - Append `NUT > Clients`, `NUT > RW Vars`, `NUT > UPSes` after existing `Commands` (in that order).

- Add typed `CmdRequest` variants for the new raw commands:
  - `btrfs filesystem commit-stats <mount>`
  - `btrfs subvolume list -a -p -c -u -q -R -t --sort=path <mount>`
  - `btrfs subvolume list -s -a -u -q -R -t --sort=path <mount>`
  - `btrfs subvolume list -d <mount>`
  - `btrfs subvolume get-default <mount>`
  - `btrfs scrub limit <mount>`
  - `btrfs quota status <mount>`
  - `btrfs qgroup show -p -c -r -e <mount>`
  - `btrfs inspect-internal list-chunks --sort=devid,pstart <mount>`
  - `upsc -c <ups>`
  - `upsrw -l <ups>`
  - `upsc -L localhost`

- Keep behavior strictly read-only:
  - Do not add `filesystem sync`, `subvolume sync`, `scrub start -r`, `check --readonly`, `upsrw -s`, or executable `upscmd <command>`.
  - Existing `NUT > Commands` remains list-only via `upscmd -l <ups>`.

- Update Browse state behavior:
  - Only `Subvolumes > List` keeps the parsed table and Enter drill-in behavior.
  - All other new Btrfs views render raw stdout/stderr only.
  - `NUT > UPSes` bypasses the `UpsNotConfigured` empty state and runs even when `ups.name` is unset (and even when `braid.ups.enable = false`); it is the discovery entry point.
  - Other NUT views that need a configured UPS name keep the existing `UPS not configured -- set \`ups.name\`...` empty state.

- Make NUT tools available to Browse regardless of `braid.ups.enable`:
  - `modules/braid/wrapper.nix`: drop the `lib.optional cfg.ups.enable` gate so `cfg.packages.nut` is always added to `toolPackages`. Without this, `NUT > UPSes` would die with `upsc: not found` on installs that have not yet enabled the UPS subsystem, which is exactly the bootstrap case the view exists to serve.
  - Cost is the closure of the pinned NUT package on every braid host; accepted as the price of making NUT-name discovery work from the TUI.

- Update documentation:
  - Refresh `manual/commands/tui.md` Browse command inventory.
  - Add a short note that `NUT > UPSes` can help discover the correct `ups.name`.

## Test Plan

- Add `CmdRequest::to_argv` unit tests for every new command variant.
- Add Browse state tests for:
  - New command/subview rows and focus skipping.
  - Each new selection mapping to the expected `CmdRequest`.
  - `NUT > UPSes` emits an effect with no `ups_config` (does not install `UpsNotConfigured`).
  - Name-required NUT views still install `UpsNotConfigured`.
  - Only `Subvolumes > List` supports parsed-table drill-in.
- Add/update Browse snapshots for the new subviews and command groups.
- Update `tests/cli/braid-tui-browse.py` for the new Subvolumes subview column:
  - Subvolumes now has subviews, so reaching the parsed list requires `l` to Subview (defaulting to `List`) then `l` to Content before `Enter` can drill in. The existing `j j` selection of Subvolumes stays the same; only the column-navigation tail changes.
- Add a Browse VM canary covering `NUT > UPSes` on an install with `braid.ups.enable = false`:
  - New VM test (e.g. `tests/cli/braid-tui-browse-ups-discovery.py`) builds a single-disk braid VM with `braid.ups.enable = false` (and no `ups.name`).
  - Launch the TUI through a tty-backed systemd canary service whose `ExecStart` is `/run/current-system/sw/bin/braid tui`, with `StandardInput = "tty-force"`, tty output/error, `TTYPath = "/dev/tty2"`, and `TERM=linux`. This exercises the installed module wrapper and its generated PATH, so the test fails if the wrapper omits NUT tools.
  - Establish membership before launching the TUI: `braid tui` calls `membership::load_membership` during startup ([cli/src/tui/mod.rs:32](/Users/dan/Code/braid/cli/src/tui/mod.rs)) and exits if `/var/lib/braid/pool.json` is missing or invalid. Mirror the existing `braid-tui-browse.py` setup -- format the disk and run `braid discover --write` so the file exists. Unlocking is unnecessary because the new `NUT > UPSes` selection does not consult pool state.
  - Start the canary service, switch to tty2, and wait for the TUI header before sending navigation keys. Drive navigation with a small `press()` helper that sleeps briefly after each key, and pair key sequences with `wait_until_tty_matches` checkpoints for the selected tab/column/row and async command output.
  - Navigate Browse to `NUT > UPSes`, and assert the rendered content is the real output of `upsc -L localhost` (e.g. matches `connect failure` / `Error:` lines from upsc, not braid's `upsc: not found` spawn-error path or the `UpsNotConfigured` empty state).
  - Register the new check in `flake.nix` per the testing doc.
- Run:
  - `cargo fmt --manifest-path cli/Cargo.toml`
  - `just test-rust`
  - `just test-vm braid-tui-browse braid-tui-browse-ups-discovery`

## Assumptions

- "Add those first" means the full recommended first batch from the prior discussion.
- New views are raw Browse surfaces only; no parser, JSON, curated summary, or new CLI command is added.
- `NUT > UPSes` intentionally narrows the previous missing-UPS empty-state rule because it is useful for diagnosing missing or wrong `ups.name`.
