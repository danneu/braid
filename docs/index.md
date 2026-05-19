---
intent: Directory of braid design docs and decision records. Read to find the authoritative doc before changing behavior.
---

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

- [principles.md](principles.md) — Thirteen canonical invariants spanning resilient boot, CLI-owned membership, safe-by-construction operations, identifier stability, NixOS-native conventions, pinned parser toolchain, pool-mutation serialization, and long-running-work announcement.
- [1-user-stories.md](1-user-stories.md) — End-to-end user workflows from first disk through pool expansion and daily operation.
- [btrfs-balance-profiles.md](btrfs-balance-profiles.md) — RAID profile conversions for data/metadata/system chunks; commands for single↔RAID1 transitions.
- [btrfs-balance-soft.md](btrfs-balance-soft.md) — The `--soft` flag optimization for resuming interrupted profile conversions without rewriting already-converted chunks.
- [btrfs-luks-sector-size.md](btrfs-luks-sector-size.md) — Why LUKS 4096-byte sector size is unnecessary — btrfs always writes 4096-byte blocks regardless.
- [claude-enospc-vs-hang.md](claude-enospc-vs-hang.md) — Two failure modes of `btrfs device remove missing` (ENOSPC vs hang) and VM reproduction techniques.
- [luks-unlock.md](luks-unlock.md) — LUKS unlock mechanisms: device naming stability, passphrase vs keyfile design, header backup workflow and messaging invariant.
- [notes-calculating-used-free-total-pool-space.txt](notes-calculating-used-free-total-pool-space.txt) — How total_bytes uses device size and RAID profile ratio; gaps with mixed-size RAID1 accounting.
- [testing.md](testing.md) — Test conventions (literal `/* */` preamble, flake.nix `checks` registration) and NixOS VM test framework gotchas (`set -euo pipefail` wrapping, f-string lint, `repro-` prefix, eval-time isolation).
- [tui-insta-guide.md](tui-insta-guide.md) — Ratatui + Insta snapshot testing workflow for TUI rendering; review cycle for snapshot changes.

## decisions/

Architecture decision records. Each has a status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

- [decisions/001-btrfs-raid1.md](decisions/001-btrfs-raid1.md) — Why btrfs RAID1 over ZFS/mdadm: checksumming, self-healing, dynamic pooling.
- [decisions/002-config-first-workflow.md](decisions/002-config-first-workflow.md) — **Superseded.** Hybrid: NixOS config as source of truth, then imperative script execution.
- [decisions/003-resilient-boot.md](decisions/003-resilient-boot.md) — Pool fails gracefully without blocking boot; data drives treated like external mounts.
- [decisions/004-single-passphrase.md](decisions/004-single-passphrase.md) — Single passphrase for all LUKS devices, verified at format time.
- [decisions/005-sane-defaults.md](decisions/005-sane-defaults.md) — Opinionated NixOS defaults via `mkDefault`; settings knowledgeable admins would enable anyway.
- [decisions/006-nix-native.md](decisions/006-nix-native.md) — Targets NixOS exclusively using standard module conventions and nixpkgs patterns.
- [decisions/007-disk-pool-management.md](decisions/007-disk-pool-management.md) — **Superseded.** Symmetric config-first workflow for add/remove/replace.
- [decisions/008-unified-cli.md](decisions/008-unified-cli.md) — **Superseded.** Unified Rust CLI replacing multiple scripts with plan/apply workflow.
- [decisions/009-safe-by-construction-reconciliation.md](decisions/009-safe-by-construction-reconciliation.md) — **Superseded.** Separate destructive formatting from repeatable reconciliation structurally.
- [decisions/010-toolchain-pinning.md](decisions/010-toolchain-pinning.md) — Pin parser-critical tools (btrfs-progs, cryptsetup, util-linux) to stable nixos-25.11.
- [decisions/011-two-phase-apply.md](decisions/011-two-phase-apply.md) — **Superseded.** LUKS pre-phase to unlock drives before pool probing.
- [decisions/012-intent-cli.md](decisions/012-intent-cli.md) — Five intent commands (`braid add/remove/replace/...`) replacing plan/apply complexity.
- [decisions/013-mount-permissions.md](decisions/013-mount-permissions.md) — Group-based mount point permissions enabling regular user write access.
- [decisions/014-alerts.md](decisions/014-alerts.md) — First-class alerting: shared alert computation across CLI, TUI, and monitor surfaces.
- [decisions/015-hdd-defaults.md](decisions/015-hdd-defaults.md) — braid optimized for HDD workloads; no flash/SSD support in operational defaults.
- [decisions/016-auto-suspend.md](decisions/016-auto-suspend.md) — Whole-system suspend-to-RAM using autosuspend for power efficiency and quiet operation.
- [decisions/017-runtime-disk-membership.md](decisions/017-runtime-disk-membership.md) — Disk membership moved from NixOS config to CLI-owned runtime state file.
- [decisions/018-systemd-lifecycle.md](decisions/018-systemd-lifecycle.md) — Thin systemd layer for unlock/mount entry points; CLI owns LUKS and btrfs operations.
- [decisions/019-inhibit-sleep.md](decisions/019-inhibit-sleep.md) — When braid should hold a systemd sleep inhibitor: only for the non-interruptible mutation window, not during prompts or reversible preflight.
- [decisions/020-ups-integration.md](decisions/020-ups-integration.md) — **Active.** Opinionated `braid.ups.*` wrapper over nixpkgs' `power.ups`: standalone USB single-host; guarantees orderly shutdown for ordinary operation + preflight reject on battery + live UPS state in `braid ups status`/TUI; mid-mutation power loss is a supported recovery case proven by the per-mutation `ups-lb-during-*` VM matrix; alert-model integration deferred to a future ADR.
- [decisions/021-wait-in-unlock.md](decisions/021-wait-in-unlock.md) — **Superseded by [Principle 13](principles.md#13-announce-long-running-work).** `braid unlock` (and `braid recover`'s shared mount tail) emitted a `[wait]` row before per-disk LUKS open and before the mount phase; promotion to a project-wide principle landed once the rest of the interactive commands complied.
- [decisions/022-dry-run-preview-model.md](decisions/022-dry-run-preview-model.md) — **Active.** Dry-run previews for migrated mutating commands render from the same typed work plans that execution consumes; `Step` is output-only.
- [decisions/023-secret-handling.md](decisions/023-secret-handling.md) — **Active.** Required types and disciplines for in-process LUKS secret material: Zeroizing typing, no BufRead in passphrase paths, hard byte caps, subprocess stdin (never argv), drop-before-fsync for generated secrets, redacted Debug, and typed passphrase boundaries.
- [decisions/024-luks-uuid-identity.md](decisions/024-luks-uuid-identity.md) — **Active.** LUKS UUID is braid's persistent disk identity; disk names and labels are presentation, by-id paths are hardware addresses, and btrfs devid is a restricted fallback.
- [decisions/025-browse-vs-curated.md](decisions/025-browse-vs-curated.md) — **Active.** `braid tui` owns the interactive UI; Browse is the raw command-output inspector while Data/Scrub are curated first-class UX.
- [decisions/026-pool-lock-rust-owned.md](decisions/026-pool-lock-rust-owned.md) — **Active.** Rust dispatch owns pool-operation locking, braid-online lifecycle synchronization, and the systemd stop coordinator.

## tool-behavior/

How external tools (btrfs-progs, cryptsetup, util-linux) actually behave in specific scenarios. These map tool output to braid's internal types. Read before modifying code that parses or reacts to tool output.

- [tool-behavior/device-disappearance.md](tool-behavior/device-disappearance.md) — Device failure states mapped to `btrfs show`, `device stats`, and `cryptsetup status` output, and how braid maps each.

## real-world/

Empirical observations from physical hardware testing. These validate the state models and assumptions in the design docs above. Each doc lists the code paths it validates — changes to those paths should prompt re-verification.

- [real-world/sata-hot-unplug.md](real-world/sata-hot-unplug.md) — SATA hot-unplug/replug behavior: btrfs, cryptsetup, and kernel state transitions on real hardware.

## btrfs-progs docs

btrfs-progs RST docs live in `reference/btrfs-progs/Documentation/`. See [AGENTS.md](../AGENTS.md) for the topic→file lookup table. Refresh with `just fetch-references`.
