---
intent: NixOS VM test framework gotchas and patterns specific to this repo. Read before writing or debugging tests under `tests/`.
---

# Testing notes

Test conventions and NixOS VM test framework reference for braid. The short three-bullet preamble contract (Intent / Why it exists / Scenario) lives in `AGENTS.md` at the repo root; everything else -- the literal preamble form, the flake.nix registration rule, framework gotchas, and patterns -- is here. For the lifecycle test suite see `tests/module/systemd-lifecycle.py`. For the Rust-level TUI view snapshot tests (insta-based, run via `just test-rust`), see [tui-snapshots.md](tui-snapshots.md).

## Conventions

### Preamble: literal `//` line-comment form

Every test's preamble is a contiguous block of `//` line comments directly above the test item.

1. **Intent** — what behavior this test verifies (or tries to verify)
2. **Why it exists** — what risk/regression this protects against
3. **Scenario** — the real-world user/system story this models, especially the concrete bug or incident that inspired the test

```rust
// Intent: one-line statement of the behavior verified.
// Why it exists: the regression risk this protects against, ideally with
//   reference to the incident or commit that prompted it.
// Scenario: the concrete real-world sequence the test models.
#[test]
fn the_test() { ... }
```

### New VM tests must register in `flake.nix`

`just test-vm` and `just test-all` build whatever is registered under `checks.<system>` in `flake.nix` -- there is no default per-test list in the justfile. When adding a new `tests/cli/*.nix` or `tests/module/*.nix`, also add a matching `pkgs.testers.nixosTest (import ./tests/cli/<name>.nix { braid = linuxCrane.braid; })` entry to `flake.nix`. An unregistered test sits in the tree but never runs under `nix flake check`.

## VM-test framework gotchas

### `just test-repro` requires the full `repro-` prefix

`just test-repro <name>` and `just test-vm <name>` pass the test name verbatim to nix as a final attribute selector. The `reproChecks` flake output is built by `filterAttrs` keeping the `repro-` prefix in the filtered set, so the attribute name passed to `just test-repro` must be exactly the name in `flake.nix`, prefix and all.

```sh
# correct
just test-repro repro-btrfs-replace-interrupted-mid-flight

# wrong -- fails with "flake ... does not provide attribute ... reproChecks.aarch64-darwin.btrfs-replace-interrupted-mid-flight"
just test-repro btrfs-replace-interrupted-mid-flight
```

The `test-vm` checks set strips entries with the `repro-` prefix, so `test-vm` test names do not have a prefix (e.g. `cli-recover-replace-completed`).

### NixOS test driver wraps every command with `set -euo pipefail`

The driver auto-prepends `set -euo pipefail` to every `machine.succeed` / `machine.execute` command before sending it to the VM. This is invisible from the test script but has real consequences for chained commands.

**Symptom:** A chain like `... ; wait $pid_loser ; echo $? > /tmp/exit-a ; ...` silently aborts when `wait` returns non-zero. The exit-code file is never written, and the next subtest assertion fails with `cat: /tmp/exit-a: No such file or directory` -- pointing at the wrong layer.

**Idiom for capturing a non-zero exit without aborting:**

```sh
ec_a=0 ; wait $pid_a || ec_a=$? ; echo $ec_a > /tmp/exit-a
```

The `||` consumes the non-zero into the variable, so errexit does not fire. Works for any command whose non-zero exit is expected (`wait`, `grep`, `diff`, etc.). This matters most in concurrent-process tests where one process is expected to exit non-zero (fail-fast lock contention, expected error paths).

### Python f-strings without placeholders fail the build-time linter

NixOS VM test scripts are linted at build time. f-strings without `{placeholder}` variables (e.g. `f"Missing foo in config"`) cause a build failure: `f-string is missing placeholders`.

In `tests/**/*.py`, never use `f"..."` without at least one `{variable}` inside. Use `"literal" + variable` for assertion messages that include dynamic values.

## Patterns

### Regression test quality

Regression tests must fail when the bug is reintroduced. Test the layer where
production failed, not a downstream parser or helper that only proves later
code works when given correct input.

For error propagation, assert the typed variant and payload. Use exact rendered
strings only for tests whose purpose is to lock `Display` or user-facing
output. If a change reclassifies an error, production and tests should call the
same mapping helper; do not hand-build the target variant in the test.

For user-visible CLI output or control-flow bugs, prefer a CLI/VM test that
drives the real command. If stdout vs stderr matters, capture them separately
with `>stdout 2>stderr`; merged streams do not pin routing. Render or preview
helpers that form a user-visible boundary need exact-output coverage for every
branch, including no-op branches.

Keep repro tests focused. If adjacent behavior already has dedicated coverage,
cite that test instead of bundling another phase into a repro whose failure
would become ambiguous.

When a dead test has a name that points at a real user-visible contract,
replace it with a real regression test by default. Deleting the dead test
turns bad coverage into no coverage.

### Live-tool behavior locks

When braid code is changed to depend on a specific external-tool behavior -- a particular exit code, a particular output wording, a particular return-value path -- mocked unit tests prove the _classifier_ is correct given the assumed behavior, but they do NOT prove the tool still behaves that way. A nixpkgs bump that changed cryptsetup's exit-code contract would silently misclassify in production while every mocked test still passed.

Whenever a plan introduces a classifier of the form `exit_code == <N>` or `stderr.contains("<wording>")` against an external tool -- or makes a decision rest on a tool's *semantics*, such as "this timestamp moves when anyone scrubs" -- identify (or add) a live-tool repro/VM test that asserts the same code, wording, or property directly. List that test in the plan's verification section as a required gate. If the live-tool test would be non-trivial to add, pause and reconsider whether the classifier is actually robust.

This is the same family as braid's parser-compatibility lanes (`just test-parsers`, `just test-rust-unstable`, see [parser compatibility](parser-compatibility.md)) -- those lock the parser against tool-output drift; a behavior-lock test locks an exit-code or wording classifier against the same drift surface. Reference example: `tests/repro/cryptsetup-close-mounted.py` asserts `exit_code == 5` for busy-close and `exit_code == 4` for already-closed, behavior-locking the assumption that `cli/src/lock.rs` retry classifier depends on. `tests/repro/btrfs-scrub-record-anchors-schedule.py` is the semantic variant: it locks that a hand-run `btrfs scrub` moves the timestamp braid schedules from, and that the record survives a reboot -- two properties of btrfs, not of braid, that the whole scrub scheduler rests on.

### VM and command test design

Before inventing VM setup for missing disks, degraded mounts, ENOSPC, hotplug,
or similar storage state, search `tests/cli/`, `tests/repro/`, and `tests/hw/`
for an existing pattern and reuse it where it fits.

When a repro test needs a scrub to stay running for a window of seconds, use
the shared helper `tests/repro/scrub_throttle_helpers.py` (deterministic
window = payload / rate via the kernel's per-device `scrub_speed_max` knob)
instead of reinventing a payload-size or block-stack throttle. Module tests
that unmount mid-window stay on dm-delay -- the knob's value dies on unmount.

Before proposing a VM test for a mutating command, search the same area for
existing notes that say a shape is infeasible, and read sibling tests to learn
which seams already exist.

For ordering invariants like "persist state before post-operation
maintenance", prefer a deterministic command-layer failure-injection test:
allow the persistence step to succeed, force the next maintenance step to
fail, then assert the persisted state is current and the journal still exists.

When code touches kernel async workers, mount-session caches, or device-layer
teardown, mocked unit tests are not enough. Run the relevant VM or repro test,
inspect full logs when it fails, and repeat timing-sensitive repros enough to
rule out a lucky pass.

For `cmd_*` boolean gates derived from multiple inputs, route both branches
through the same injected seam and test the matrix cells that distinguish the
intended gate from plausible wrong gates.

For one-off sequenced or stateful command-test behavior, prefer a file-local
runner or wrapper over widening the shared `MockRunner`. Reserve shared runner
API changes for behavior that many tests need.

When removing sleep wall-time from tests, inject a sleeper dependency. Do not
use `#[cfg(test)]` to zero a production timing constant whose value is part of
the behavior.

### Eval-time test isolation: disable, don't stub

When an eval-time test (`lib.evalModules` in isolation) breaks because of a new NixOS option dependency, **disable** the unrelated feature in the test config rather than expanding the fake module surface with stubs.

Stubbing options (e.g. adding `options.users`) makes the test less isolated and can mask future accidental dependencies on unrelated NixOS top-level options. Disabling the feature that introduced the dependency keeps the test focused.

When fixing eval-time test failures caused by new module dependencies, first check if the dependency comes from a feature the test doesn't need. If so, set that feature's config to its "off" value (e.g. `poolAccessGroup = null`) instead of adding option stubs.
