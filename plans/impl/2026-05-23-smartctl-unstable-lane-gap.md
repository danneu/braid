# Plan: close the smartctl unstable-lane gap honestly

## Context

A code-review finding flagged HIGH severity drift risk for parser-critical
tool versions, claiming four parsers (`parse_lsblk_json`,
`parse_cryptsetup_luks_dump`, `parse_smartctl_health`,
`parse_btrfs_scrub_status_per_device`) have no unstable-lane fixture
coverage and proposing more prose in AGENTS.md.

Investigation showed three of the four are already covered:
`cli/tests/support/golden_common.rs:64-82, 323-333, 347-379` define
golden tests that get `include!`-ed into both
`cli/tests/golden_nixos_25_11.rs` and `cli/tests/golden_nixos_unstable.rs`,
and the matching fixtures exist in `cli/tests/fixtures/nixos-unstable/`.
`cargo test --test golden_nixos_unstable -- --list` confirms
`golden_lsblk_json`, `golden_cryptsetup_luks_dump`,
`golden_btrfs_scrub_per_device_finished`, and
`golden_btrfs_scrub_per_device_running` all run.

The real gap is narrower:
- **Only smartctl is genuinely unstable-uncovered.** No `smartctl-*.json`
  files exist in `cli/tests/fixtures/nixos-unstable/`, and
  `tests/capture-tool-fixtures.py` issues zero `smartctl` invocations.
  `parse_smartctl` is TUI-only (`cli/src/tui/probe.rs:255-258`) and never
  reached by any VM test. `parse_smartctl_selftest_log` runs in the doctor
  VM test (which `test-all-unstable` covers), but `tests/cli/braid-doctor.py:15-37`
  only asserts report shape (`smart_self_test` rows exist with unique
  subjects), not parsed selftest semantics -- so it won't catch upstream
  JSON shape drift.
- **AGENTS.md:247 is misleading.** It claims
  `test-rust-unstable` "covers the full parser surface
  (btrfs/cryptsetup/util-linux/smartctl/NUT)". For smartctl that is false:
  no fixtures, no tests.
- **`tests/cli/tool-versions.py` has no smartmontools provenance check.**
  Lines 11-35 verify that btrfs/cryptsetup/findmnt/lsblk/mountpoint/upsc
  all resolve under `/nix/store/` on the VM's PATH and that the binary's
  self-reported version matches `<pkg>.version` from the same Nix
  evaluation that built the VM. smartmontools is absent from both loops,
  so an ambient binary shadowing `smartctl` on the VM PATH or a
  package/binary version mismatch involving `smartctl` would not be
  caught even though every other pinned tool is. (The braid wrapper's
  PATH injection is separately tested only for `upsc` at
  `tool-versions.py:76-81`; this plan does not extend that check.)
- **The "catches drift" framing in decision 010 is inaccurate.**
  `docs/design/decisions/010-toolchain-pinning.md:49` claims the
  `tool-versions` test "catches drift" on a nixpkgs bump. It does not:
  `expected-versions.json` is derived from `pkgs.<tool>.version` in the
  same evaluation the VM is built from, so both sides advance together
  when nixpkgs moves. The test catches provenance/configured-version
  alignment, not version drift relative to upstream.

This plan closes the two real gaps with the smallest honest change:
extend the `tool-versions` provenance/configured-version smoke test to
cover smartmontools (parity with the other four pinned tools), then
correct the docs so the smartctl coverage limitation and the actual
guarantee of `tool-versions` are both explicit rather than oversold.
Hand-mirroring smartctl fixtures or parameterizing the inline selftest
tests was considered and rejected -- the smartctl fixtures are either
one-time physical-drive captures or hand-authored synthetic JSON, so
copying them into a second lane adds no drift detection (the same parser
hits identical input both lanes).

## Changes

### 1. Add smartmontools to the `tool-versions` provenance + configured-version smoke test

Bring smartmontools up to the same coverage as btrfs/cryptsetup/util-linux/nut.
This catches two things: (a) VM PATH provenance -- `command -v smartctl`
in the test VM resolves to a `/nix/store/` path, so an ambient or
otherwise-shadowed binary cannot quietly take over; and (b)
configured-package vs binary version alignment -- the binary's
`--version` output matches `pkgs.smartmontools.version` from the same
evaluation. It does not exercise the braid wrapper's PATH injection for
`smartctl` (the only wrapper-path subtest today is upsc-only, see
`tests/cli/tool-versions.py:76-81`), and it cannot detect that nixpkgs
has moved smartmontools to a new version (the expected version is read
from the same `pkgs` evaluation); that is the manual obligation already
documented in `cli/tests/fixtures/nixos-25.11/README.md`.

**`tests/cli/tool-versions.nix`** -- two-line additions mirroring the
existing pattern:

- Add `pkgs.smartmontools` to `environment.systemPackages` (line 25-32)
  so `smartctl` is on PATH in the test VM.
- Add `smartmontools = pkgs.smartmontools.version;` to the
  `expected-versions.json` block (line 37-42).

Also rewrite the file's preamble (lines 1-10) so it stops describing the
test as a drift gate. The current `What:` line names only
btrfs-progs/cryptsetup/util-linux (already stale -- nut is missing) and
the `Why:` line says the test "catches version drift". Replace with
wording that lists the full current tool set (btrfs-progs, cryptsetup,
util-linux, nut, smartmontools) and describes what the test actually
guarantees:

```nix
# Test: tool provenance + configured-version alignment
#
# What: Validates that runtime tools (btrfs-progs, cryptsetup,
# util-linux, nut, smartmontools) resolve to /nix/store/ paths via the
# VM's PATH and that each binary's self-reported version matches
# pkgs.<tool>.version from this same evaluation. Separately, validates
# that the braid wrapper can resolve upsc with an empty ambient PATH
# (upsc only -- the other tools have no wrapper-path subtest today).
#
# Why: Catches ambient binaries shadowing the pinned toolchain on the
# VM PATH and package/binary version mismatches (e.g. a patched binary
# whose --version string drifts from pkgs.<tool>.version). Does NOT
# catch nixpkgs version moves -- expected versions read from the same
# pkgs evaluation that builds the VM, so both sides advance together.
# Drift relative to upstream is gated by the manual fixture-refresh
# workflow documented in cli/tests/fixtures/nixos-25.11/README.md and
# docs/design/decisions/010-toolchain-pinning.md.
#
# Dependencies: braid module (options.nix, cli.nix) must wire
# cfg.packages correctly.
```

**`tests/cli/tool-versions.py`** -- new subtest after the nut block
(after line 35), following the same prefix-match pattern used for nut and
cryptsetup. Verify the exact `smartctl --version` first-line format
against a running VM during implementation; the assertion form will be:

```python
with subtest("smartmontools version"):
    version = machine.succeed("smartctl --version").strip().splitlines()[0]
    exp = f"smartctl {expected['smartmontools']}"
    assert version.startswith(exp), f"expected prefix {exp!r}, got {version!r}"
```

Also add `smartctl` to the provenance loop at line 11 so it's verified to
resolve under `/nix/store/`.

### 2. Make AGENTS.md honest about smartctl unstable coverage

**`AGENTS.md:246-247`** -- rewrite the "Unstable lane" bullet list to
drop the false `smartctl` claim and add a dedicated bullet explaining
why. Proposed replacement:

```markdown
- `just test-all-unstable` -- VM tests against nixos-unstable. Covers
  CLI-reachable parsers against live tool output but does not cover the
  full parser surface (TUI-only parsers, unused parsers, smartctl).
- `just capture-all-fixtures-unstable` + `just test-rust-unstable` --
  covers btrfs/cryptsetup/util-linux/NUT against unstable tool output via
  golden fixtures. Missing fixtures fail (not skip).
- **smartctl is stable-only by design.** VM virtio disks do not emit
  useful SMART data, so the smartctl fixtures cannot be captured from the
  VM pipeline. `smartctl-sata-with-temperature.json` is a one-time
  physical-drive capture; the `smartctl-selftest-*.json` fixtures are
  hand-authored (see `cli/tests/fixtures/nixos-25.11/README.md`). The
  `tool-versions` VM test checks that `smartctl` resolves to a
  `/nix/store/` path on the VM's PATH and that its self-reported version
  matches the configured `pkgs.smartmontools.version`; it does not
  exercise the braid wrapper's PATH injection for `smartctl` and it
  does not detect nixpkgs version bumps (both sides advance together
  with the evaluation). On any nixpkgs bump that touches smartmontools,
  review and refresh the stable smartctl fixtures by hand.
```

### 3. Add `cli/tests/fixtures/nixos-unstable/README.md`

Mirror the stable-lane README so the per-lane caveats live next to the
fixtures. Short and direct:

```markdown
Golden-file fixtures captured from a nixos-unstable VM by
`just capture-all-fixtures-unstable`. Non-authoritative; they exist so
upstream output changes are visible in git history.

**No smartctl fixtures by design.** VM virtio disks do not emit useful
SMART data. The smartctl parsers are exercised only against
`cli/tests/fixtures/nixos-25.11/smartctl-*.json` (a physical-drive SATA
capture and hand-authored selftest fixtures). The `tool-versions` VM
test verifies `smartctl` provenance and configured-package version but
does not detect nixpkgs version moves -- on any smartmontools nixpkgs
bump, review and refresh the stable smartctl fixtures by hand.
```

### 4. Correct `docs/design/decisions/010-toolchain-pinning.md`

Two sentences in this doc currently carry the same "tests catch drift"
overstatement that AGENTS.md does. Fix both in the same edit so the
authoritative decision doc, AGENTS.md, and the unstable README all agree.

**Step 3 of "Upgrading tools" (line 49) currently says:**

> 3. Run `make test` -- the version-assertion test (`tool-versions`) catches drift.

Replace with:

```markdown
3. Run `make test` -- the `tool-versions` VM test verifies that each
   pinned tool resolves to a `/nix/store/` path on the VM's PATH and
   that its self-reported version matches `pkgs.<tool>.version` from
   the same evaluation. It does not detect that nixpkgs has moved a
   tool to a new version (both sides advance together); use steps 4
   and 5 as the actual drift gate.
```

**The "Operational escape hatch" paragraph (line 25) currently ends:**

> The override takes precedence; if the newer version changes output format, parser tests will catch it.

That sentence carries the same false confidence in two ways: parser
tests only catch an override's output-format change if a fixture exists
at the override's version, and the existing `just capture-all-fixtures`
/ `just test-rust` recipes do not accept arbitrary `braid.packages.*`
overrides -- they build fixed flake checks (`justfile:147`,
`flake.nix:417`) whose `pkgs` comes from the flake's nixpkgs input, not
from any per-system module override. Replace the trailing sentence with:

```markdown
The override takes precedence. Operator-set `braid.packages.*` overrides
sit outside braid's committed parser contract: the standard
fixture-capture and golden-test recipes build fixed flake checks against
the flake's `pkgs`, so they do not validate an arbitrary override.
Treating an override as supported requires a maintainer to reproduce the
fixture-refresh workflow under a temporary local input swap (e.g.
`--override-input nixpkgs` on the capture/test commands, or a local
flake edit) at the override's package version, then re-run
`just test-rust` against the resulting fixtures. Operators who skip
this step are running unverified parser inputs.
```

This brings the authoritative upgrade docs and the operational
override-escape-hatch into line with AGENTS.md and the new unstable
README, so neither the upgrade procedure nor the override workflow
inherits the false confidence the plan is removing elsewhere.

## What we are not doing, and why

- **Not** hand-mirroring smartctl fixtures into `nixos-unstable/`. The
  parser is the same code, so identical input in both lanes produces
  identical output -- zero canary value.
- **Not** capturing live `smartctl -H -A --json /dev/vdb` from the
  unstable VM. Uncertain payload (virtio disks expose minimal SMART), and
  even if the JSON envelope is stable, the canary only catches changes to
  the error/minimal shape, not real-drive payload changes.
- **Not** parameterizing the inline selftest tests in
  `cli/src/parse/smartctl.rs:389` (the hardcoded `FIXTURE_DIR`). Same
  reason as the hand-mirror rejection -- the synthetic fixtures don't
  change with tool versions.

## Critical files

- `tests/cli/tool-versions.nix` -- add smartmontools to systemPackages
  and expected-versions.json; rewrite the file preamble (lines 1-10) to
  name the current tool set and describe provenance/configured-version
  alignment instead of drift detection.
- `tests/cli/tool-versions.py` -- add provenance entry + version subtest.
- `AGENTS.md` -- rewrite the Unstable-lane bullets at lines 246-247.
- `cli/tests/fixtures/nixos-unstable/README.md` -- new file.
- `docs/design/decisions/010-toolchain-pinning.md` -- rewrite step 3 of
  the Upgrading-tools list (line 49) to describe what `tool-versions`
  actually catches, and the trailing sentence of the
  "Operational escape hatch" paragraph (line 25) so package overrides
  require explicit fixture refresh before being treated as supported.

## Verification

1. Run `just test-vm tool-versions` on stable and confirm the new
   smartmontools subtest passes alongside the existing four. To prove
   the assertion actually checks something, deliberately mutate the
   `pkgs.smartmontools.version` reference in `tool-versions.nix` to a
   stale string and confirm the subtest fails with a clear "expected
   prefix X, got Y" message; revert the mutation.
2. Run `just test-vm tool-versions --unstable` and confirm it passes.
   It is expected to pass even when unstable's smartmontools differs
   from stable's, because both sides advance together; the run is a
   parity check, not a drift gate. (If it fails, that means the
   `--version` output format itself changed between releases -- a real
   parser-adjacent signal worth investigating.)
3. Build the docs and confirm no broken cross-links: `mdbook build docs`
   (per AGENTS.md, `mdbook-linkcheck` runs as part of the build and
   fails CI on a broken cross-link).
4. Re-read AGENTS.md, the new unstable README, decision 010 (both the
   "Upgrading tools" step and the "Operational escape hatch" paragraph),
   and the `tool-versions.nix` preamble from a cold-eye reader's
   perspective: the smartctl coverage limitation and the actual
   guarantee of `tool-versions` should both be impossible to miss, no
   document should still describe the test as catching version drift,
   and `braid.packages.*` overrides should clearly require manual
   fixture refresh.
