---
intent: NixOS VM test framework gotchas and patterns specific to this repo. Read before writing or debugging tests under `tests/`.
---

# Testing notes

Test conventions and NixOS VM test framework reference for braid. The short three-bullet preamble contract (Intent / Why it exists / Scenario) lives in [AGENTS.md](../AGENTS.md); everything else -- the literal preamble form, the flake.nix registration rule, framework gotchas, and patterns -- is here. For the lifecycle test suite see `tests/module/systemd-lifecycle.py`.

## Conventions

### Preamble: literal `/* ... */` form

Every test's preamble is a literal `/* ... */` block comment, not `//` line comments. Many existing tests use `//` -- those are grandfathered, not the standard. Do not copy that style for new tests.

```rust
/*
 * Intent: one-line statement of the behavior verified.
 * Why it exists: the regression risk this protects against, ideally with
 *   reference to the incident or commit that prompted it.
 * Scenario: the concrete real-world sequence the test models.
 */
#[test]
fn the_test() { ... }
```

Reference example to model on: `lock_retries_busy_close_then_succeeds` in `cli/src/lock.rs`.

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

### Live-tool behavior locks

When braid code is changed to depend on a specific external-tool behavior -- a particular exit code, a particular output wording, a particular return-value path -- mocked unit tests prove the *classifier* is correct given the assumed behavior, but they do NOT prove the tool still behaves that way. A nixpkgs bump that changed cryptsetup's exit-code contract would silently misclassify in production while every mocked test still passed.

Whenever a plan introduces a classifier of the form `exit_code == <N>` or `stderr.contains("<wording>")` against an external tool, identify (or add) a live-tool repro/VM test that asserts the same code/wording directly. List that test in the plan's verification section as a required gate. If the live-tool test would be non-trivial to add, pause and reconsider whether the classifier is actually robust.

This is the same family as braid's parser-compatibility lanes (`just test-parsers`, `just test-rust-unstable`, see [AGENTS.md](../AGENTS.md#parser-compatibility)) -- those lock the parser against tool-output drift; a behavior-lock test locks an exit-code or wording classifier against the same drift surface. Reference example: `tests/repro/cryptsetup-close-mounted.py` asserts `exit_code == 5` for busy-close and `exit_code == 4` for already-closed, behavior-locking the assumption that `cli/src/lock.rs` retry classifier depends on.

### Eval-time test isolation: disable, don't stub

When an eval-time test (`lib.evalModules` in isolation) breaks because of a new NixOS option dependency, **disable** the unrelated feature in the test config rather than expanding the fake module surface with stubs.

Stubbing options (e.g. adding `options.users`) makes the test less isolated and can mask future accidental dependencies on unrelated NixOS top-level options. Disabling the feature that introduced the dependency keeps the test focused.

When fixing eval-time test failures caused by new module dependencies, first check if the dependency comes from a feature the test doesn't need. If so, set that feature's config to its "off" value (e.g. `storageGroup = null`) instead of adding option stubs.
