# AGENTS.md

## Project: braid

braid is a Rust CLI tool + NixOS module for managing a NixOS-based NAS of full-disk-encrypted drives (luks) in a btrfs raid1 array.

braid wraps luks + btrfs to provide higher level UX to make things easier, more accessible, and less error-prone for people just trying to manage their NAS without fiddling or reading manpages to do everything.

## Example

```
Physical drives:
  /dev/sda → LUKS ─┐
  /dev/sdb → LUKS ─┼─ single btrfs RAID1 → /mnt/storage
  /dev/sdc → LUKS ─┘

Unlock:
  NAS powers on → boots to login (pool offline)
  → ssh user@nas → sudo braid unlock
  → LUKS drives open → btrfs assembles → pool online
```

## The Stack

- **NixOS** — declarative, reproducible system configuration
- **LUKS** — passphrase-based full disk encryption (keys never stored on disk)
- **btrfs RAID1** — checksumming filesystem with automatic self-healing from redundant copies; dynamic add/remove drives

## Layout

- `cli/src/` — Rust CLI (clap commands, TUI in `tui/`)
- `modules/braid/` — NixOS module (options, systemd units, storage config)
- `tests/` — NixOS VM tests (`.py` scripts, `module/` NixOS configs, `hw/` hardware canary tests)
- `docs/decisions/` — architecture decision records
- `docs/btrfs-docs/` — local btrfs-progs reference (RST)
- `scripts/` — helper scripts (fetch btrfs docs, destroy pool)
- `reference/` — vendored upstream source (`btrfs-progs`, `autosuspend`) for code-level reference

## Systemd Lifecycle

Systemd lifecycle design: [`docs/decisions/systemd-lifecycle.md`](docs/decisions/systemd-lifecycle.md). Read before modifying units, the wrapper, or writing systemd-related tests.

## No backwards compatibility

braid is unreleased software. Never add migration paths, compatibility shims, or legacy support. If a format or interface changes, change it everywhere — old versions are not a concern.

## Architecture Authority

Design principles and invariants live in [`docs/principles.md`](docs/principles.md). Detailed rationale, rejected alternatives, and historical context live in [`docs/decisions/`](docs/decisions/).

Any change to behavior or invariants must update those docs. Code that contradicts a principle is wrong — fix the code or update the principle with rationale.

Decision docs must include an explicit status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

## User Guide

[`README.md`](README.md) is the end-user guide. Keep it updated when adding features or changing behavior. Style: brief, cookbook-like — short descriptions with copy-paste examples. Not reference material.

## References

- [User stories](docs/1-user-stories.md) — full UX walkthrough from first disk to third

## btrfs Documentation Reference

Local copy of the official btrfs-progs documentation lives in [`docs/btrfs-docs/`](docs/btrfs-docs/). Fetch/refresh it with `scripts/fetch-btrfs-docs.sh`. Use these docs before searching the web. Entry point: `docs/btrfs-docs/index.rst`.

| Topic | File(s) |
|---|---|
| Adding/removing devices | `Volume-management.rst`, `btrfs-device.rst` |
| Device replacement | `btrfs-replace.rst` |
| Rebalancing | `Balance.rst`, `btrfs-balance.rst` |
| RAID profiles (RAID1 etc.) | `mkfs.btrfs.rst` (search for "profiles") |
| Mount options | `btrfs-man5.rst` |
| Scrub / self-healing | `Scrub.rst` |
| Filesystem limits & storage model | `btrfs-man5.rst` |
| Administration overview | `Administration.rst` |

## Git Commits

The first line of a commit message must not be capitalized (e.g. `fix the foo bug`, not `Fix the foo bug`).

## Commands

- `just test` — Run NixOS VM tests (excludes repro tests).
- `just test -v` — Run tests with full VM logs.
- `just test test1 test2` — Run one or more specific checks.
- `just test test1 -v` — Run specific checks with verbose output.
- `just test-repro` — Run repro tests only (same flags as `test`).
- `just test-all` — Run all tests including repro.
- `just test-rust` — Run Rust unit tests (`cargo test`).

**Test verbosity:** Run tests without `-v` by default. Only add `-v` to a specific failing test when the non-verbose output doesn't explain the failure. Never run `just test -v` (all tests verbose) — it produces too much output to be useful.

## Test Conventions

Every individual test must start with a block comment explaining this:

1. **Intent** — what behavior this test verifies (or tries to verify)
2. **Why it exists** — what risk/regression this protects against
3. **Scenario** — the real-world user/system story this models, especially the concrete bug or incident that inspired the test

## Development Approach: TDD with NixOS VM Tests

Write failing tests first, confirm they fail for the expected reasons, then implement the NixOS config to make them pass.

- **Test framework:** NixOS VM tests (`nixos/lib/testing-python.nix`)
- **Runs on macOS:** Requires `nix.linux-builder.enable = true` in nix-darwin. Tests are `checks.aarch64-darwin`.
- **Virtual disks:** `virtualisation.emptyDiskImages` creates throwaway virtual drives.

---

## Plan Review Protocol

When reviewing a plan:

- List findings ordered by severity.
- For each finding: state issue, impact, and include one recommended fix.
- Prescriptions must be singular: do not present multiple options in the report.
- After listing findings, assess the overall plan viability.
- If you think there's an even better + simpler + more robust solution, tell the
  user so that they can consider pivoting to a new, better plan.

Decision rule:

- For each finding, consider the best resolutions and their trade-offs internally, then choose the best solution.
- If multiple open-ended solutions exist, brainstorm with the user until one
  solution is agreed.
- After alignment, report only that agreed solution.

Example (single finding):

- High: Plan makes `braid status` mutate disk-map state.
  Impact: A read command causes side effects, which breaks safety expectations and complicates debugging.
  Recommended fix: Keep `braid status` read-only; perform disk-map reconciliation only in explicit mutating commands (`add`, `remove`, `remove-missing`, `replace`).
