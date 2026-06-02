# Plan: fix the "Upgrading tools" procedure in the toolchain-pinning ADR

## Context

The "Upgrading tools" procedure in the toolchain-pinning decision doc has two
problems, both surfaced by a review of `docs/design/decisions/010-toolchain-pinning.md:46-57`:

1. **Stale command (the original finding).** Step 3 says `make test`, but braid
   has no Makefile -- the test runner is `just` (`justfile:67`, `test-vm *args`).
   A maintainer following the steps runs a command that does not exist. The
   `make test` wording is a leftover from braid's pre-`just` toolchain, where
   `make test` meant "all NixOS VM tests pass"
   (`plans/impl/2026-01-01-predated/intent-refactor.md:577`); it was copied
   verbatim into the ADR in commit `ae640d73`.

2. **Incomplete validation (review follow-up).** Even with the command fixed,
   the list under-specifies the drift gate. Steps 4-5 only say "capture fixtures"
   and "update parser tests if output format changed" -- they never name the
   commands that actually prove the refreshed parser contract. The canonical
   sequence is documented elsewhere (`docs/dev/overview.md:113-122` and
   `AGENTS.md:304-308`): `just capture-all-fixtures`, `just test-rust`,
   `just test-parsers`, `just test-vm`. The ADR's parallel list drifted out of
   sync with it -- which is exactly how the `make test` staleness arose.

This is shipped reference material (the mdBook ADR tree) and the architecture
authority for tool pinning, so the procedure must be both runnable and complete.

Intended outcome: the "Upgrading tools" list mirrors the canonical
fixture-refresh + parser-validation workflow, and reframes `tool-versions` as
the provenance check covered by the VM lane rather than the lone named VM test.

## The change

One edit, in `docs/design/decisions/010-toolchain-pinning.md`: replace the
numbered list at lines 46-57 (the `### Upgrading tools` heading through old
step 5). The `NUT specifically:` / `ethtool specifically:` paragraphs that
follow (lines 59-61) stay unchanged -- they already reference `just test-rust`,
`just test-parsers`, `just capture-ups-fixtures`, and `tool-versions`, and
remain consistent with the rewrite.

Replace with:

```markdown
### Upgrading tools

A nixpkgs bump can move parser-critical tools to new output formats, so an
upgrade must refresh fixtures and re-run every parser-validation lane -- not
just confirm tool provenance. These steps mirror the canonical sequence in
[dev/overview.md](../../dev/overview.md) ("Refresh fixtures and run tests");
keep the two in sync.

1. Bump the nixpkgs input to the next stable release and run `nix flake update nixpkgs`.
2. Refresh fixtures: `just capture-all-fixtures` writes golden files under
   `cli/tests/fixtures/nixos-<release>/` (with `upsc/` holding the
   `capture-ups-fixtures` outputs). `just capture-all-fixtures-unstable` is the
   unstable-lane mirror.
3. Run the parser-validation lanes, updating parsers/tests for any output that
   changed:
   - `just test-rust` -- golden-fixture parser tests.
   - `just test-parsers` -- live-tool parser canary.
   - `just test-vm` -- VM suite. Its `tool-versions` check verifies provenance:
     each pinned tool resolves to a `/nix/store/` path on the VM's PATH and its
     self-reported version matches `pkgs.<tool>.version` from the same
     evaluation. Provenance only -- `tool-versions` does not detect that nixpkgs
     moved a tool to a new version (both sides advance together), so the fixture
     and parser tests above are the actual drift gate. Run it alone with
     `just test-vm tool-versions` for a quick provenance-only check.
```

### Notes on the rewrite

- **All four canonical commands are named** (`capture-all-fixtures`,
  `test-rust`, `test-parsers`, `test-vm`), matching `docs/dev/overview.md:117-122`.
- **`tool-versions` is reframed** as a provenance check *within* the VM lane,
  not the only named VM test -- per the review.
- **The user's earlier command choice is preserved.** `just test-vm tool-versions`
  (chosen in planning as the focused provenance command) survives as the "quick
  provenance-only check," now sitting inside the full validation sequence rather
  than standing in for it.
- **No dangling references.** Old step 3's "use steps 4 and 5 as the actual
  drift gate" is reworded to "the fixture and parser tests above"; the NUT/ethtool
  paragraphs reference command names, not step numbers, so renumbering 5->3 is
  safe. Implementer should still grep the doc for any other "step N" reference
  before saving.
- **Cross-reference added** to `dev/overview.md` to make the duplication
  explicit and fight future drift (the root cause of this finding). The link
  target is tracked and in `docs/SUMMARY.md:87`, so `mdbook-linkcheck2` will
  validate it. This is a minor addition beyond the literal review fix.

## Out of scope (intentionally not touched)

The same `make test` / `make test-*` wording appears in historical plan records
(`plans/impl/2026-05-23-smartctl-unstable-lane-gap.md:189,194`;
`plans/impl/2026-01-01-predated/intent-refactor.md:577`;
`plans/impl/2026-01-01-predated/plan-disk-map.md:36-37,105-108`). These are
point-in-time records of what was planned, not procedures anyone follows (one
set is under a `2026-01-01-predated/` archive dir). Editing them rewrites
history for no reader benefit. The only live correctness issue is the ADR.

## Verification

1. **Stale command gone, tracked-source scoped:**
   `git grep -n -E 'make (test|build|check|all|run|install)' -- docs README.md AGENTS.md CLAUDE.md`
   -- returns nothing after the edit. Use `git grep` (not `rg`) so the search is
   tracked-files-only: the gitignored `docs/book/` rendered output contains a
   stale `make test` (`docs/book/html/print.html`), which `rg --no-ignore` or a
   global rg config would surface even after the source is fixed. (Plain `rg`
   respects `.gitignore` here, but `git grep` is unambiguous and matches the
   repo's tracked-file-inventory convention.)
2. **Doc builds and the new cross-link resolves:** `mdbook build docs` --
   `mdbook-linkcheck2` validates the `../../dev/overview.md` link and the ADR
   renders.
3. **Named commands are real:** `just --list` shows `capture-all-fixtures`,
   `test-rust`, `test-parsers`, `test-vm`; `tool-versions` is a buildable check
   registered at `flake.nix:452` (`tests/cli/tool-versions.nix` / `.py`). No need
   to run the VM suite for a docs edit -- existence is the gate.
