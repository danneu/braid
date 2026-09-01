---
intent: How braid validates its tool-output parsers against version drift -- stable and unstable lanes, fixture-refresh triggers, and the smartctl/ethtool hand-authored-fixture caveats. Read on any nixpkgs bump of a parser-critical tool.
---

# Parser compatibility

braid parses output from btrfs-progs, cryptsetup, util-linux, smartmontools, NUT, and ethtool. The fragile parser/safety set (btrfs-progs, cryptsetup, NUT, smartmontools, ethtool) is pinned by default; util-linux is host-provided because `lsblk --json` is a stable structured contract. All parsed tools remain fixture-covered because parser contracts can still drift when tool versions change. Two validation lanes exist:

## Stable lane (pinned and stable contracts)

- `just test-parsers` — focused parser canary. Exercises selected CLI-reachable parsers against live tool output in VMs (including `braid-status-ups`, the NUT canary); it is not an exhaustive inventory of every CLI parser path.
- `just test-rust` — validates golden fixtures for the full parser set, including `parse_upsc`. Fixture-backed coverage stays current only after running `just capture-all-fixtures` when parser-critical tool versions change (e.g. nixpkgs bump).
- Fixture refresh is a separate obligation: `just test-parsers` passing does not guarantee the TUI-only `parse_lsblk_json`, the unused `parse_btrfs_scrub_status_per_device`, or `parse_cryptsetup_luks_dump` are compatible with the current toolchain. `parse_cryptsetup_luks_dump` is CLI-reachable through `braid replace`'s existing-LUKS target-capacity preflight. The registered `replace-new-already-luks` and `replace-enroll-existing-luks` VM checks exercise that fail-closed path against live cryptsetup output in the full VM suite, but neither check belongs to the focused `test-parsers` recipe.
- `parse_smartctl` (the SMART health parser) is reachable from both the TUI and the `braid status` CLI command, so it is no longer TUI-only. It is **still not** covered by the live VM canary, though: virtio disks emit no usable SMART, so `just test-parsers` cannot exercise it. Its drift canary is the stable-only smartctl golden fixture (see the smartctl-fixtures note below).
- Fixtures in `cli/tests/fixtures/nixos-26.05/` are committed and authoritative. NUT fixtures live in `cli/tests/fixtures/nixos-26.05/upsc/` (and the unstable mirror); they are produced by `just capture-ups-fixtures`, which boots a dedicated NUT VM with per-state `dummy-ups` drivers (see `tests/capture-ups-fixtures.nix`).
- **smartctl fixtures are stable-only by design.** VM virtio disks do
  not emit useful SMART data, so `just capture-all-fixtures` does not
  regenerate `smartctl-sata-with-temperature.json`,
  `smartctl-nvme-healthy.json`, or `smartctl-selftest-*.json`.
  `smartctl-sata-with-temperature.json` is a one-time physical-drive
  capture; the NVMe and self-test fixtures are hand-authored (see
  `cli/tests/fixtures/nixos-26.05/README.md`). The `tool-versions` VM
  test checks that `smartctl` resolves to a `/nix/store/` path on the
  VM's PATH and that its self-reported version matches
  `pkgs.smartmontools.version`, but it does not detect nixpkgs version
  bumps because both sides advance together. On any nixpkgs bump that
  touches smartmontools, manually review and refresh
  `smartctl-selftest-*.json` against the new
  `ata_smart_self_test_log.standard` JSON shape,
  `smartctl-nvme-healthy.json` against the new
  `nvme_smart_health_information_log` shape, and
  `smartctl-sata-with-temperature.json` against the new
  health/temperature JSON shape (`smart_status`, `temperature`,
  `ata_smart_attributes`).
- **ethtool WoL fixtures are hand-authored / no-live-capture.** VM
  virtio NICs do not emit useful Wake-on-LAN data, so
  `just capture-all-fixtures` does not regenerate ethtool output. The
  doctor `wake_on_lan` parser is covered by hand-authored Rust unit
  fixtures, and wrapper provenance is covered by the override-based VM
  tests in `tool-versions` and `braid-auto-suspend`.

Parser-critical tool versions are the pinned `nixpkgs` versions of `btrfs-progs`, `cryptsetup`, `nut`, `smartmontools`, and `ethtool`, plus host-provided util-linux's `lsblk --json` stable contract. Treat any change to the `nixpkgs` node in `flake.lock`, any `flake.nix` change that alters the `nixpkgs` input, or any change to `braid.packages.{btrfsProgs,cryptsetup,utilLinux,nut,smartmontools,ethtool}` as a required fixture-refresh event.

When parser-critical tool versions change, run:

1. `just capture-all-fixtures`
2. `just test-rust`
3. `just test-parsers`
4. On any bump that moves btrfs-progs or the kernel (any `nixpkgs` node change moves both): `just test-repro repro-btrfs-scrub-limit-bounds-rate` -- the live-tool behavior lock for the bounded-rate scrub throttle the scrub-window repro tests rest on. This step is manual and required: `repro-*` checks are excluded from `.#checks`, so no CI workflow runs it.

## Unstable lane (tracked forecast)

Early-warning lane for upstream parser/output drift. Unstable failures signal upcoming changes, not a contract violation. Fixtures in `cli/tests/fixtures/nixos-unstable/` are committed so upstream output changes are visible in git history, but they are non-authoritative.

- `just test-all-unstable` -- VM tests against nixos-unstable. Covers
  CLI-reachable parsers against live tool output but does not cover the
  full parser surface (TUI-only parsers, unused parsers, smartctl).
- `just capture-all-fixtures-unstable` + `just test-rust-unstable` --
  covers btrfs/cryptsetup/util-linux/NUT against unstable tool output via
  golden fixtures. Missing fixtures fail (not skip).
- **smartctl and ethtool have no unstable fixtures.** Unstable
  capture/test coverage intentionally covers btrfs/cryptsetup/util-linux/NUT
  only; see the Stable lane for why smartctl fixtures are stable-only and
  how to refresh them on smartmontools bumps, and why ethtool WoL output
  is hand-authored instead of live-captured.

Full unstable canary workflow:

1. `just test-all-unstable`
2. `just capture-all-fixtures-unstable`
3. `just test-rust-unstable`
