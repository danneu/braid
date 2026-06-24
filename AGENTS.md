# AGENTS.md

braid is a Rust CLI + NixOS module for managing a NixOS NAS: full-disk-encrypted
drives (LUKS) in a btrfs RAID1 array. It wraps LUKS + btrfs behind higher-level UX
so people can run a NAS without fiddling with manpages or error-prone low-level
commands. GitHub: https://github.com/danneu/braid

**Stack:**

- **NixOS** -- declarative, reproducible system config
- **LUKS** -- full-disk encryption
- **btrfs RAID1** -- checksumming, self-healing, dynamic add/remove drives
- **systemd** -- lifecycle and service management, comes with NixOS

## Layout

- `cli/src/` -- Rust CLI (clap commands; TUI in `tui/`)
- `modules/braid/` -- NixOS module (options, systemd units, storage config)
- `tests/` -- NixOS VM tests (`.py` scripts, `module/` configs, `hw/` hardware canaries)
- `docs/` -- unified mdBook: `guides/` and `commands/` (end-user), `design/principles.md`
  and `design/decisions/` (authority), `internals/` (implementation notes), `dev/`
  (contributor docs). TOC: [`SUMMARY.md`](docs/SUMMARY.md); landing: [`index.md`](docs/index.md).
- `scripts/` -- helper scripts
- `reference/` -- vendored upstream source for reading, not shipped

## Architecture & authority

Invariants and design principles are law: [`principles.md`](docs/design/principles.md).
Rationale, rejected alternatives, and history live in [`decisions/`](docs/design/decisions/).
Any change to behavior or an invariant must update them. Code that contradicts a
principle is wrong -- fix the code, or change the principle with rationale. Every
ADR carries a status: `Draft`, `Active`, `Superseded`, or `Deprecated`.

Always reach for the ideal, robust, simple, most correct solution -- regardless of
scope, refactor, or backwards-compatibility cost.

## Read before you touch

- systemd units, the wrapper, or systemd tests -> [ADR 018 systemd-lifecycle](docs/design/decisions/018-systemd-lifecycle.md)
- systemd unit hardening or sandbox directives -> [ADR 033 systemd-unit-hardening](docs/design/decisions/033-systemd-unit-hardening.md)
- Rust CLI subprocess spawning or child environment -> [ADR 034 subprocess-environment-discipline](docs/design/decisions/034-subprocess-environment-discipline.md)
- dry-run, preview, or mutating command planning/execution -> [ADR 022 dry-run-preview-model](docs/design/decisions/022-dry-run-preview-model.md)
- mutation code (invariant placement, fail-closed policy, residual guards, state enums) -> [safety-heuristics.md](docs/dev/safety-heuristics.md)
- writing or reviewing a plan -> [planning-hygiene.md](docs/dev/planning-hygiene.md)
- editing a frozen (Superseded/Deprecated) ADR or a `## See` section -> [doc-citations.md#decision-doc-references](docs/dev/doc-citations.md#decision-doc-references) (enforced by `scripts/docs/check-see-paths.py`)
- `doctor`/`status`/`unlock` recovery messaging or the LUKS header-backup workflow -> [luks-unlock.md](docs/internals/luks-unlock.md#header-backup-workflow-and-messaging)

## Conventions (always)

- **CLI output is ASCII only.** In user-facing output (errors, help, TUI strings,
  `echo` lines) use ASCII, not Unicode substitutes: `--` for em/en-dash, `'`/`"`
  for curly quotes, `...` for ellipsis, `x` for the multiplication sign. Rendering
  Unicode (arrows, box-drawing, degree sign, spinners) is fine. Enforced by
  `scripts/docs/check-output-ascii.py` over `cli/src/**/*.rs` and `modules/**/*.nix` echo lines
  (comments and tests exempt).
- **Commits:** Conventional Commits; first line lowercase (`fix the foo bug`, not `Fix ...`).
- **File citations:** cite `path#symbol` (code, as a code span) or `path#heading-slug`
  (markdown link) -- never line numbers. Details: [doc-citations.md](docs/dev/doc-citations.md);
  citing vendored `reference/` code: [reference-source.md](docs/dev/reference-source.md#citing-reference-code).
- **Doc comments:** every top-level or `pub`/`pub(crate)` Rust CLI item gets a `///`
  saying why it exists at that boundary (intent/invariant/ownership), not what the
  signature already says. Skip list + Good/Bad catalog: [doc-comments.md](docs/dev/doc-comments.md).

## Docs

[`SUMMARY.md`](docs/SUMMARY.md) is the single TOC for the unified mdBook tree --
check it before searching the codebase for context. Cross-links are validated by
`mdbook-linkcheck2` during `just docs-build`; a broken link fails CI.

Keep [`README.md`](README.md) (brief cookbook, copy-paste examples) in sync with the
`docs/guides/` and `docs/commands/` mdBook reference when behavior changes.

Before web-searching a tool's behavior or output format, read the vendored upstream
source in `reference/` (nixpkgs-pinned shallow clones + Rust crate sources; refresh
with `just fetch-references`). Per-tool inventory: [reference-source.md](docs/dev/reference-source.md).

## Testing

TDD with NixOS VM tests: write failing tests first, confirm they fail for the right
reason, then implement the module config to make them pass. Framework:
`nixos/lib/testing-python.nix`; runs on macOS via `nix.linux-builder.enable` (checks
are `aarch64-darwin`); throwaway disks via `virtualisation.emptyDiskImages`.

Every test opens with a `//` preamble: **Intent** (behavior verified), **Why it
exists** (regression guarded), **Scenario** (real-world story / inspiring incident).
Literal form, the `flake.nix` `checks` registration rule, and framework gotchas:
[testing.md](docs/dev/testing.md).

**Parser compatibility:** braid parses btrfs-progs, cryptsetup, NUT,
smartmontools, ethtool, and util-linux's `lsblk --json`. The five fragile
parser/safety tools are pinned; util-linux is host-provided through a stable JSON
contract, but stays fixture-covered. Any change to the `nixpkgs` node in
`flake.lock` (or to `braid.packages.{btrfsProgs,cryptsetup,utilLinux,nut,smartmontools,ethtool}`)
is a fixture-refresh event: `just capture-all-fixtures` -> `just test-rust` ->
`just test-parsers`. Lanes and caveats: [parser-compatibility.md](docs/dev/parser-compatibility.md).

## Commands

Every recipe lives in the [`justfile`](justfile) with an explanatory comment (run
`just --list` for the summary).
