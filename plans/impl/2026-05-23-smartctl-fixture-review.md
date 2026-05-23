# Plan: surface hand-authored smartctl-selftest fixture review in AGENTS.md

## Context

A reviewer of `cli/src/parse/smartctl.rs` flagged that `parse_smartctl_selftest_log` is protocol-sensitive and that the hand-authored fixtures (`cli/tests/fixtures/nixos-25.11/smartctl-selftest-*.json`) require manual review on smartmontools bumps. That fact is already documented in `cli/tests/fixtures/nixos-25.11/README.md:13-19` and is partially captured in `AGENTS.md`'s **Unstable lane** bullet (`AGENTS.md:297-308`). But the AGENTS.md "Stable lane" subsection -- which owns the bump recipe (`just capture-all-fixtures` -> `just test-rust` -> `just test-parsers`) -- does not surface the exception.

The risk: an agent reading the Stable lane top-to-bottom during a smartmontools bump will not see the hand-authored caveat. They have to scroll into the Unstable lane to discover it. `capture-all-fixtures` does not regenerate `smartctl-selftest-*.json` (VM virtio disks emit no useful self-test logs), `test-rust` validates against the committed fixtures regardless of the new tool's output, and `test-parsers` does not exercise the self-test parser path. So an agent following the Stable lane recipe verbatim gets all green and never reviews the hand-authored fixtures.

Intended outcome: the canonical smartctl fixture exception lives in the Stable lane (where the fixtures actually live and the bump recipe is documented), and the Unstable lane shrinks to a short pointer plus only the unstable-specific fact.

## Proposed change

Single-file edit to `AGENTS.md`. Two parts:

**1. Add a new bullet to the Stable lane (around `AGENTS.md:277`, after the NUT-fixture bullet and before the "Parser-critical tool versions are..." paragraph).** This bullet becomes the canonical smartctl fixture exception. Content to cover:

- smartctl fixtures are stable-only by design (VM virtio disks emit no useful SMART data).
- `smartctl-sata-with-temperature.json` is a one-time physical-drive capture; `smartctl-selftest-*.json` are hand-authored.
- `just capture-all-fixtures` does not regenerate either.
- `tool-versions` VM test confirms `smartctl` resolves on PATH and its self-reported version matches `pkgs.smartmontools.version`, but does not detect nixpkgs version bumps (both sides advance together).
- On any nixpkgs bump that touches smartmontools, manually review and refresh these fixtures: `smartctl-selftest-*.json` against the new `ata_smart_self_test_log.standard` JSON shape, and `smartctl-sata-with-temperature.json` against the new health/temperature JSON shape (`smart_status`, `temperature`, `ata_smart_attributes`). See `cli/tests/fixtures/nixos-25.11/README.md`.

**2. Replace the Unstable lane smartctl bullet (`AGENTS.md:297-308`) with a short pointer.** It should only say what is unstable-specific: smartctl has no unstable fixtures, see the Stable lane bullet for the why. All hand-authored / capture-pipeline / smartmontools-bump prose moves to (1) and gets deleted here.

Also update the parenthetical in the existing `test-all-unstable` bullet (`AGENTS.md:291-293`) -- "(TUI-only parsers, unused parsers, smartctl)" stays as-is; that listing is still accurate.

No code, README, justfile, or fixture changes.

## Critical files

- `AGENTS.md` -- one bullet added inside Stable lane, one bullet rewritten in Unstable lane.

## Verification

- Re-read `AGENTS.md`'s Stable lane top-to-bottom after the edit. The smartctl-selftest hand-authored caveat must be discoverable without leaving the Stable lane.
- Re-read the Unstable lane. The remaining smartctl bullet must contain only the unstable-specific fact (no smartctl fixtures captured here) plus a pointer; no duplicated maintenance procedure.
- Confirm the cross-link `cli/tests/fixtures/nixos-25.11/README.md` resolves from repo root.
- No tests, builds, or other commands needed -- AGENTS.md is not part of the mdBook tree, so `mdbook-linkcheck` does not validate it.
