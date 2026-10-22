# braid slices for hunt

How hunt lists braid's slices. Every slice also looks for correctness
bugs and simpler designs. Tool behavior (btrfs, cryptsetup, systemd,
smartctl, NUT, util-linux, the kernel) is read in `reference/` before
the web ([reference-source.md](../../docs/dev/reference-source.md)).

## Commands

List every public leaf command of the braid CLI from the clap
definitions reachable from `cli/src/main.rs`, using effective clap
names. Skip hidden commands and parent-only commands (`ups` is a parent;
`ups status` is a leaf).

- Id: `command:<name>`, spaces as hyphens (`command:ups-status`).
- Files: the dispatch site, the command module, and its
  `docs/commands/` page.
- Review the command end to end: races, TOCTOU, unsafe operation order,
  wrong LUKS/btrfs/systemd semantics, parse errors at tool boundaries,
  error messages, and whether its tests would fail if the bug came
  back. Cover argument parsing, shared planner/executor code, parsers,
  NixOS module wiring, tests, and docs.

## Docs

List every page linked from `docs/SUMMARY.md`.

- Id: `docs:<path relative to docs/>` (`docs:guides/auto-unlock.md`).
- Files: the page itself.
- Review accuracy against the code (stale flags, defaults, output),
  contradictions with principles, ADRs, README, or other pages, shipped
  behavior left undocumented, and verbosity. braid docs are terse;
  bloat is a finding with a concrete cut.

## Acceptance commands

Acceptance commands are `just` recipes run as whole commands (`just
test-rust`, `just clippy`, `just test-vm braid-add`, `just check-docs`).
Name a regression proof's lane: Rust unit test, parser golden fixture,
parser canary, or NixOS VM test. VM recipes share a host-wide gate
([testing.md](../../docs/dev/testing.md#vm-booting-recipes-share-a-host-wide-gate)).
