---
name: New VM tests must register in flake.nix, not justfile
description: braid's `just test-vm` dispatches to `checks` entries in flake.nix; adding a .py/.nix test without a matching `<name> = pkgs.testers.nixosTest (...)` block leaves it unreachable
type: feedback
originSessionId: a0261adb-fa0c-4f46-a5d5-0ddfd093f3e4
---
When a plan adds a new NixOS VM test under `tests/cli/` or `tests/module/`,
the plan MUST list `flake.nix` among the critical files and include the
`pkgs.testers.nixosTest (import ./tests/cli/<name>.nix { braid =
linuxCrane.braid; })` registration snippet. `just test-vm` and
`just test-all` in `justfile:65/81` build whatever is registered under
`checks` in `flake.nix` (see the `replace-*` block around
`flake.nix:236-299`). There is no default per-test list in the justfile.

**Why:** An unregistered test file sits in the tree but never runs under
`nix flake check`, so a plan's "new coverage" never lands. Spotted on
plan-the-fix-simplicity-peaceful-kahn -- proposed `just test-vm
replace-missing-soft-balance` without a flake registration.

**How to apply:** Whenever a plan or patch introduces a new
`tests/cli/*.nix` or `tests/module/*.nix`, confirm (and call out
explicitly) the `flake.nix` `checks.<system>` registration. Treat a
missing registration the same as a missing test.
