# AGENTS.md

## Project: braid

Github: https://github.com/danneu/braid

braid is a Rust CLI tool + NixOS module for managing a NixOS-based NAS of full-disk-encrypted drives (luks) in a btrfs raid1 array.

braid wraps luks + btrfs to provide higher level UX to make things easier, more accessible, and less error-prone for people just trying to manage their NAS without fiddling or reading manpages to do everything.

## The Stack

- **NixOS** — declarative, reproducible system configuration
- **LUKS** — passphrase-based full disk encryption (keys never stored on disk)
- **btrfs RAID1** — checksumming filesystem with automatic self-healing from redundant copies; dynamic add/remove drives

## Layout

- `cli/src/` — Rust CLI (clap commands, TUI in `tui/`)
- `modules/braid/` — NixOS module (options, systemd units, storage config)
- `tests/` — NixOS VM tests (`.py` scripts, `module/` NixOS configs, `hw/` hardware canary tests)
- `docs/` — unified mdBook docs (single TOC at `docs/SUMMARY.md`, landing at `docs/index.md`)
  - `guides/`, `commands/` — end-user material
  - `design/principles.md`, `design/decisions/` — architecture authority
  - `internals/` — implementation notes (luks-unlock, tool behavior, btrfs deep-dives)
  - `dev/` — contributor docs (development workflow, testing, TUI snapshots)
- `scripts/` — helper scripts
- `reference/` — upstream source checkouts for reading, not shipped. Full inventory in [`docs/dev/reference-source.md`](docs/dev/reference-source.md). Refresh with `just fetch-references`.

## General guidelines

- Always consider the ideal, robust, simple, most correct solution regardless of
  its scope cost, refactoring cost, and backwards compatibility cost.

## Systemd Lifecycle

Systemd lifecycle design: [`docs/design/decisions/018-systemd-lifecycle.md`](docs/design/decisions/018-systemd-lifecycle.md). Read before modifying units, the wrapper, or writing systemd-related tests.

## Architecture Authority

Design principles and invariants live in [`docs/design/principles.md`](docs/design/principles.md). Detailed rationale, rejected alternatives, and historical context live in [`docs/design/decisions/`](docs/design/decisions/).

Any change to behavior or invariants must update those docs. Code that contradicts a principle is wrong — fix the code or update the principle with rationale.

Decision docs must include an explicit status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

Before modifying dry-run, preview, or mutating command planning/execution, read [`docs/design/decisions/022-dry-run-preview-model.md`](docs/design/decisions/022-dry-run-preview-model.md).

## Planning and Review Hygiene

Re-read central files before planning, derive rename/refactor inventories from `git ls-files` + `rg`, and verify recovery recipes against current `cmd_*`/`plan_*` code: [`docs/dev/planning-hygiene.md`](docs/dev/planning-hygiene.md). Read before writing or reviewing a plan.

## Mutation Safety Heuristics

Invariant placement, fail-closed policy, hard-vs-`debug_assert!` residual guards, and state-enum discipline for mutating commands: [`docs/dev/safety-heuristics.md`](docs/dev/safety-heuristics.md). Read before touching mutation code.

## User Guide

End-user material lives in two places: [`README.md`](README.md) is the cookbook-style overview
(brief, copy-paste examples), and `docs/guides/` + `docs/commands/` is the mdBook reference
(formerly `manual/`). Keep both in sync when adding features or changing behavior. Style for
README.md: brief, cookbook-like — short descriptions with copy-paste examples. Not reference
material.

## Documentation

[`docs/SUMMARY.md`](docs/SUMMARY.md) is the TOC for the unified docs tree (end-user guides,
commands, design principles, ADRs, internals, contributor docs). [`docs/index.md`](docs/index.md)
is the landing page. Check `SUMMARY.md` before searching the codebase for context. All cross-links
inside `docs/` are validated by `mdbook-linkcheck2` during `just docs-build` (configured in
`docs/book.toml`) -- a broken cross-link fails CI.

### Reference source

Before searching the web for a tool's behavior or output format, read the vendored
upstream source in `reference/` (shallow clones at nixpkgs-pinned versions, plus
Rust crate sources; refresh with `just fetch-references`). The full per-tool
inventory -- what each checkout holds and what to read it for -- and the btrfs docs
topic table live in [`docs/dev/reference-source.md`](docs/dev/reference-source.md).

## File References

Cite files by `path#symbol` (code, as a plain code span) or `path#heading-slug` (markdown link), never by line number: [`docs/dev/doc-citations.md`](docs/dev/doc-citations.md). Read before writing an ADR or doc cross-reference. For citing vendored upstream code under `reference/`, see [`docs/dev/reference-source.md`](docs/dev/reference-source.md#citing-reference-code).

## Decision Doc References

Do not rewrite a frozen (Superseded/Deprecated) ADR's body or `## See` section to track current code; the `> Superseded by ...` banner is the forward pointer. The `## See` rules (enforced by `scripts/docs/check-see-paths.py`): [`docs/dev/doc-citations.md`](docs/dev/doc-citations.md#decision-doc-references). Read before editing a decision doc.

## Git Commits

Use Conventional Commits-style commit messages. The first line must not be
capitalized (e.g. `fix the foo bug`, not `Fix the foo bug`).

## CLI Output Style

Use plain ASCII, not typographic Unicode substitutes, in all user-facing CLI
output -- error messages, help text, TUI strings, shell `echo` lines. Banned
substitutes and their ASCII forms: em-dash and en-dash -> `--`; curly single
and double quotes -> `'` and `"`; ellipsis -> `...`; multiplication sign -> `x`.
These render poorly over SSH and in non-UTF-8 locales. Rendering Unicode
(arrows, box-drawing, the degree sign, spinner glyphs) is fine -- only the
plain-ASCII substitutes are banned.

Enforced by `scripts/docs/check-output-ascii.py` (a lexical scan of
`cli/src/**/*.rs` string/help context and `modules/**/*.nix` `echo` lines, run
in CI and via `just check-output-ascii`); comments and test code are out of
scope.

Example: `pool is not mounted -- nothing to acknowledge`

For the LUKS header backup workflow and the messaging invariant for `doctor`/`status`/`unlock` recovery hints, see [`docs/internals/luks-unlock.md`](docs/internals/luks-unlock.md#header-backup-workflow-and-messaging).

## Doc Comments

When adding a top-level `fn`/type/module/trait or `pub`/`pub(crate)` item in the Rust CLI, add a `///` doc comment justifying why it exists at that boundary (intent/invariant/ownership), not the signature. Skip list and Good/Bad catalog: [`docs/dev/doc-comments.md`](docs/dev/doc-comments.md). Rust CLI only.

## Commands

- `just test-vm` — Run NixOS VM tests (excludes repro tests).
- `just test-vm -v` — Run tests with full VM logs.
- `just test-vm test1 test2` — Run one or more specific checks.
- `just test-vm test1 -v` — Run specific checks with verbose output.
- `just test-repro` — Run repro tests only (same flags as `test-vm`).
- `just test-all` — Run all tests including repro.
- `just test-parsers` — Run parser compatibility canary (CLI parsers against live VM tool output).
- `just test-rust` — Run Rust unit tests (`cargo test`). The CLI crate's package name is `braid-cli` (not `braid`); prefer `just test-rust` over `cargo test -p <name>` so you don't have to remember.
- `just test-all-unstable` — Run all VM tests (including repro) against nixos-unstable.
- `just capture-all-fixtures` — Capture all stable fixtures (base + progress).
- `just capture-all-fixtures-unstable` — Capture all unstable fixtures (base + progress).
- `just test-rust-unstable` — Run golden parser tests against unstable fixtures.

`just test-vm` and `just test-repro` accept `--unstable` to run VM tests against nixos-unstable (e.g. `just test-vm hello-world --unstable`). For fixture capture and Rust golden tests, use the dedicated `-unstable` recipes above.

**Test verbosity:** Run tests without `-v` by default. Only add `-v` to a specific failing test when the non-verbose output doesn't explain the failure. Never run `just test-vm -v` (all tests verbose) — it produces too much output to be useful.

**Test scope:** Default to focused runs (`just test-vm test1 test2`) -- the full suite takes 20-30 minutes. Only run the unscoped `just test-vm` for changes with broad blast radius (systemd lifecycle, pool lock, mount/unmount, module-wide refactors) or right before handing work back to the user on a substantial change. For small, localized changes, run only the tests that exercise the touched code path.

If a full-suite run surfaces one specific failing VM test, fix and verify that
test plus any touched siblings. Do not autonomously rerun the full suite after
the focused fix; tell the user it is ready for their full-suite rerun.

## Test Conventions

Every individual test starts with a `//` line-comment preamble with three labeled sections:

1. **Intent** — what behavior this test verifies (or tries to verify)
2. **Why it exists** — what risk/regression this protects against
3. **Scenario** — the real-world user/system story this models, especially the concrete bug or incident that inspired the test

For the literal preamble form, the flake.nix `checks` registration rule for new VM tests, and NixOS VM test framework gotchas, see [`docs/dev/testing.md`](docs/dev/testing.md).

## Development Approach: TDD with NixOS VM Tests

Write failing tests first, confirm they fail for the expected reasons, then implement the NixOS config to make them pass.

- **Test framework:** NixOS VM tests (`nixos/lib/testing-python.nix`)
- **Runs on macOS:** Requires `nix.linux-builder.enable = true` in nix-darwin. Tests are `checks.aarch64-darwin`.
- **Virtual disks:** `virtualisation.emptyDiskImages` creates throwaway virtual drives.

## Parser Compatibility

braid parses tool output from btrfs-progs, cryptsetup, util-linux, NUT,
smartmontools, and ethtool; these parsers break when tool versions drift. Treat any
change to the `nixpkgs` node in `flake.lock` (or to
`braid.packages.{btrfsProgs,cryptsetup,utilLinux,nut,smartmontools,ethtool}`) as a
required fixture-refresh event: run `just capture-all-fixtures`, then `just test-rust`,
then `just test-parsers`.

The stable and unstable validation lanes, the smartctl/ethtool hand-authored-fixture
caveats, and the full unstable workflow are in
[`docs/dev/parser-compatibility.md`](docs/dev/parser-compatibility.md).
