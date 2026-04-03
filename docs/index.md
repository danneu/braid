# docs/

## Frontmatter

Docs should have YAML frontmatter. Add incrementally: when creating a new doc or substantively editing an existing one.

- `intent`: (required) What this doc is focused on documenting and why; when to read it.

```yaml
---
intent: Map device-disappearance states to btrfs, cryptsetup, and kernel output and how braid maps each to internal types. Read before modifying probe, monitor, or alert code.
---
```

## Top-level docs

- [principles.md](principles.md) — Five canonical invariants: resilient boot, CLI-owned membership, safe operations, single passphrase, stable identifiers.
- [1-user-stories.md](1-user-stories.md) — End-to-end user workflows from first disk through pool expansion and daily operation.
- [btrfs-balance-profiles.md](btrfs-balance-profiles.md) — RAID profile conversions for data/metadata/system chunks; commands for single↔RAID1 transitions.
- [btrfs-balance-soft.md](btrfs-balance-soft.md) — The `--soft` flag optimization for resuming interrupted profile conversions without rewriting already-converted chunks.
- [btrfs-luks-sector-size.md](btrfs-luks-sector-size.md) — Why LUKS 4096-byte sector size is unnecessary — btrfs always writes 4096-byte blocks regardless.
- [claude-enospc-vs-hang.md](claude-enospc-vs-hang.md) — Two failure modes of `btrfs device remove missing` (ENOSPC vs hang) and VM reproduction techniques.
- [luks-unlock.md](luks-unlock.md) — LUKS unlock mechanisms: device naming stability and passphrase vs keyfile design considerations.
- [notes-calculating-used-free-total-pool-space.txt](notes-calculating-used-free-total-pool-space.txt) — How total_bytes uses device size and RAID profile ratio; gaps with mixed-size RAID1 accounting.
- [tui-insta-guide.md](tui-insta-guide.md) — Ratatui + Insta snapshot testing workflow for TUI rendering; review cycle for snapshot changes.

## decisions/

Architecture decision records. Each has a status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

- [decisions/alerts.md](decisions/alerts.md) — First-class alerting: shared alert computation across CLI, TUI, and monitor surfaces.
- [decisions/auto-suspend.md](decisions/auto-suspend.md) — Whole-system suspend-to-RAM using autosuspend for power efficiency and quiet operation.
- [decisions/btrfs-raid1.md](decisions/btrfs-raid1.md) — Why btrfs RAID1 over ZFS/mdadm: checksumming, self-healing, dynamic pooling.
- [decisions/config-first-workflow.md](decisions/config-first-workflow.md) — **Superseded.** Hybrid: NixOS config as source of truth, then imperative script execution.
- [decisions/disk-pool-management.md](decisions/disk-pool-management.md) — **Superseded.** Symmetric config-first workflow for add/remove/replace.
- [decisions/hdd-defaults.md](decisions/hdd-defaults.md) — braid optimized for HDD workloads; no flash/SSD support in operational defaults.
- [decisions/intent-cli.md](decisions/intent-cli.md) — Five intent commands (`braid add/remove/replace/...`) replacing plan/apply complexity.
- [decisions/mount-permissions.md](decisions/mount-permissions.md) — Group-based mount point permissions enabling regular user write access.
- [decisions/nix-native.md](decisions/nix-native.md) — Targets NixOS exclusively using standard module conventions and nixpkgs patterns.
- [decisions/resilient-boot.md](decisions/resilient-boot.md) — Pool fails gracefully without blocking boot; data drives treated like external mounts.
- [decisions/runtime-disk-membership.md](decisions/runtime-disk-membership.md) — Disk membership moved from NixOS config to CLI-owned runtime state file.
- [decisions/safe-by-construction-reconciliation.md](decisions/safe-by-construction-reconciliation.md) — Separate destructive formatting from repeatable reconciliation structurally.
- [decisions/sane-defaults.md](decisions/sane-defaults.md) — Opinionated NixOS defaults via `mkDefault`; settings knowledgeable admins would enable anyway.
- [decisions/single-passphrase.md](decisions/single-passphrase.md) — Single passphrase for all LUKS devices, verified at format time.
- [decisions/systemd-lifecycle.md](decisions/systemd-lifecycle.md) — Thin systemd layer for unlock/mount entry points; CLI owns LUKS and btrfs operations.
- [decisions/toolchain-pinning.md](decisions/toolchain-pinning.md) — Pin parser-critical tools (btrfs-progs, cryptsetup, util-linux) to stable nixos-25.11.
- [decisions/two-phase-apply.md](decisions/two-phase-apply.md) — **Superseded.** LUKS pre-phase to unlock drives before pool probing.
- [decisions/unified-cli.md](decisions/unified-cli.md) — **Superseded.** Unified Rust CLI replacing multiple scripts with plan/apply workflow.

## tool-behavior/

How external tools (btrfs-progs, cryptsetup, util-linux) actually behave in specific scenarios. These map tool output to braid's internal types. Read before modifying code that parses or reacts to tool output.

- [tool-behavior/device-disappearance.md](tool-behavior/device-disappearance.md) — Device failure states mapped to `btrfs show`, `device stats`, and `cryptsetup status` output, and how braid maps each.

## real-world/

Empirical observations from physical hardware testing. These validate the state models and assumptions in the design docs above. Each doc lists the code paths it validates — changes to those paths should prompt re-verification.

- [real-world/sata-hot-unplug.md](real-world/sata-hot-unplug.md) — SATA hot-unplug/replug behavior: btrfs, cryptsetup, and kernel state transitions on real hardware.

## btrfs-progs docs

btrfs-progs RST docs live in `reference/btrfs-progs/Documentation/`. See [AGENTS.md](../AGENTS.md) for the topic→file lookup table. Refresh with `just fetch-references`.
