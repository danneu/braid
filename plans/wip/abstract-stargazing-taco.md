# Plan: Parser Compatibility Canary

## Context

braid has 18 parsers consuming output from btrfs-progs, cryptsetup, util-linux, and smartmontools. When nixpkgs is bumped, any of these tools could change their output format and break braid's parsers.

Existing VM tests already exercise 15 of 18 parsers through real braid commands, but this coverage is scattered across 6+ tests with no clear "did parsers break?" entry point. The remaining 3 parsers (TUI-only or VM-impossible) are only testable via captured fixtures.

**Goal:** a single `just test-parsers` entry point that answers "are all parsers OK?" plus golden test coverage for every parser.

## Parser coverage map

| Parser | Used by | Live test via | Fixture test |
|--------|---------|--------------|--------------|
| `parse_findmnt_json` | status, idle | braid-status-rust | golden_findmnt_json |
| `parse_btrfs_filesystem_show` | status, add | braid-status-rust | golden_btrfs_show |
| `parse_cryptsetup_status` | status, add | braid-status-rust | golden_cryptsetup_status |
| `parse_cryptsetup_luks_uuid` | status (probe) | braid-status-rust | golden_cryptsetup_luks_uuid |
| `parse_btrfs_df_json` | status | braid-status-rust | golden_btrfs_df_json |
| `parse_btrfs_filesystem_usage` | status | braid-status-rust | golden_btrfs_usage |
| `parse_btrfs_device_usage` | status | braid-status-rust | golden_btrfs_device_usage |
| `parse_btrfs_scrub_status` | status, idle | braid-status-rust, braid-idle | golden_btrfs_scrub_* |
| `parse_btrfs_balance_status` | status, idle | braid-status-during-balance | golden_btrfs_balance_* |
| `parse_btrfs_device_stats` | status, monitor | braid-status-rust | golden_btrfs_device_stats |
| `parse_btrfs_replace_status` | idle | braid-idle | — |
| `parse_cryptsetup_luks_label` | add, discover | braid-discover, braid-add-disk | — |
| `parse_btrfs_subvolume_list` | browse | braid-browse | **MISSING** |
| `parse_lsblk_json` | TUI only | — (TUI-only) | golden_lsblk_json |
| `parse_cryptsetup_luks_dump` | TUI only | — (TUI-only) | **MISSING** |
| `parse_smartctl_health` | TUI only | — (VM-impossible) | golden_smartctl_nvme_healthy |
| `parse_btrfs_scrub_status_per_device` | unused | — | **MISSING (x2)** |

## Plan

### Step 1: Fill 4 missing golden fixture tests

Add to `cli/tests/golden_nixos_25_11.rs` using the existing `golden_test!` macro. Fixtures already exist; tests are missing.

| Test name | Fixture file | Parser |
|-----------|-------------|--------|
| `golden_cryptsetup_luks_dump` | `cryptsetup-luks-dump.json` | `parse_cryptsetup_luks_dump` |
| `golden_btrfs_scrub_per_device_finished` | `btrfs-scrub-per-device-finished.txt` | `parse_btrfs_scrub_status_per_device` |
| `golden_btrfs_scrub_per_device_running` | `btrfs-scrub-per-device-running.txt` | `parse_btrfs_scrub_status_per_device` |
| `golden_btrfs_subvolume_list` | `btrfs-subvolume-list.txt` | `parse_btrfs_subvolume_list` |

After this step, every parser has at least one test against real captured output.

**Note:** This is coverage backfill, not red-green TDD. The parsers already work and the fixtures already exist — the gap is missing test coverage, not broken parsers. These tests should pass immediately. If any fail, that's a real finding worth investigating.

### Step 2: Add `just test-parsers` recipe

Add a recipe to the `justfile` that runs the existing parser-sensitive VM tests as a group:

```just
# Run parser compatibility canary tests (CLI parsers against live tool output)
test-parsers *args:
    just test braid-status-rust braid-status-during-balance braid-idle braid-discover braid-browse {{args}}
```

This aggregates the 5 existing tests that collectively exercise all 15 CLI-reachable parsers. No new VM test, no duplicated coverage.

**What `just test-parsers` covers:**
- 15 of 18 parsers exercised against live tool output in VMs
- All parser state variants already tested (idle, completed, running, degraded, paused)

**What it does not cover (and why):**
- `parse_lsblk_json` — TUI-only, no CLI command exercises it. Covered by golden fixture test (Step 1).
- `parse_cryptsetup_luks_dump` — TUI-only, no CLI command exercises it. Covered by golden fixture test (Step 1).
- `parse_smartctl_health` — virtio disks have no SMART. Covered by golden fixture test.
- `parse_btrfs_scrub_status_per_device` — not called by any CLI command. Covered by golden fixture test (Step 1).

The golden fixture tests catch drift when `just capture-fixtures` is re-run against a new NixOS version. They are a separate fixture-refresh obligation on toolchain bumps — `just test-parsers` does not claim to cover them.

### Step 3: Document in AGENTS.md

Add a `## Parser Compatibility` section to AGENTS.md with these constraints:
- `just test-parsers` — CLI parser canary. Covers only CLI-reachable parsers (15 of 18) against live VM tool output.
- `just test-rust` — validates golden fixtures for all 18 parsers, but fixture-backed coverage stays current only after running `just capture-fixtures` and `just capture-progress-fixtures` when parser-critical tool versions change (e.g. nixpkgs bump).
- Fixture refresh is a separate obligation: `just test-parsers` passing does not guarantee TUI-only parsers (`parse_lsblk_json`, `parse_cryptsetup_luks_dump`, `parse_smartctl_health`) or unused parsers (`parse_btrfs_scrub_status_per_device`) are compatible with the current toolchain.

## Files to modify

| File | Change |
|------|--------|
| `cli/tests/golden_nixos_25_11.rs` | Add 4 golden tests |
| `justfile` | Add `test-parsers` recipe |
| `AGENTS.md` | Add parser compatibility section |

## Verification

1. Write the 4 golden tests → `just test-rust` → confirm they pass
2. Add `test-parsers` recipe → `just test-parsers` → confirm all 5 VM tests pass
3. Audit: every `pub use` in `cli/src/parse/mod.rs` is exercised by either `test-parsers` (live) or a golden fixture test
