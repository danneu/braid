# Pin `nix-fast-build` as a project-local dev tool

## Context

`just test-fast` currently calls a bare `nix-fast-build` binary on PATH, which
fails on a fresh checkout (`command not found`). We want this recipe to work
out of the box for any contributor without a global install, and we want the
version of `nix-fast-build` pinned to something we control rather than
whatever the user has in their profile.

`nix-fast-build` is already in our pinned `nixos-25.11` nixpkgs input (v1.3.0
-- which has every flag this recipe uses: `-j`, `--eval-workers`,
`--eval-max-memory-size`, `--no-link`, `--skip-cached`, `--no-nom`). So we
don't need a new flake input -- exposing the existing nixpkgs derivation as a
flake `packages` output is enough. The pin then rides on the existing
`flake.lock` entry for nixpkgs, with no new attack surface and no new lock
entry to audit.

Secondary goal: fix the recipe's resource tuning while we're here. The
current `-j 8 --eval-workers 4` (with the 4 GiB-per-worker default cap) can
let eval alone reach ~16 GiB, layered on top of 8 concurrent QEMU VMs, on a
32 GiB M1. We mirror `_build-checks`' tuned `--max-jobs 7` and cap eval
worker RAM.

## Changes

### 1. `flake.nix` -- expose `nix-fast-build` as a package output

In `packagesFor` (currently flake.nix:62-79), add the nixpkgs derivation to
the base attribute set so both `aarch64-darwin` and `x86_64-linux` get it:

```nix
{
  inherit (craneFor system) braid-cli-unwrapped;
  nix-fast-build = pkgs.nix-fast-build;
}
```

Nothing else in the flake needs to change. `forAllSystems` already covers
both target systems, `pkgs` is already in scope, and `meta.mainProgram` is
`"nix-fast-build"` on the upstream package so `nix run` resolves correctly.

### 2. `justfile` -- rewrite `test-fast` to use the flake package

Replace the recipe body at justfile:87-99. Two substantive edits:

- Call `nix run .#nix-fast-build -- ...` instead of a bare `nix-fast-build`
  binary lookup. The pin comes from our flake's locked nixpkgs.
- Tune flags: `-j 7` (match `_build-checks`), keep `--eval-workers 4`, add
  `--eval-max-memory-size 2048` (cap each eval worker at 2 GiB instead of the
  4 GiB default), add `--skip-cached` (free win for incremental runs), and
  bake in `--no-nom` to drop the multi-line dep-graph UI in favor of a
  single-line progress counter.

Refreshed comment block notes that the tool is fetched via the flake (so the
"requires nix-fast-build" warning is no longer accurate) and that the `-j`
value mirrors `_build-checks`' Mac-RAM-tuned `--max-jobs 7`.

Not included on purpose: a stderr `grep -v "waiting for lock on"` filter.
That filter would also swallow other nix messages on stderr; if it becomes
annoying it's a one-line follow-up.

## Verification

1. `nix run .#nix-fast-build -- --help` -- prints help and exits 0 (proves
   the flake output works and the binary resolves).
2. `nix flake check --no-build` -- the new package output evaluates without
   error.
3. `just test-fast` -- runs to completion on a clean tree. Compare wall-clock
   against `time just test-vm` (cache-warmed both sides) to confirm the
   parallel-eval win is real.

## Follow-ups (out of scope here)

- Document `just test-fast` in `AGENTS.md`'s Commands section -- it's
  currently absent.
- Consider adding a `--rebuild` passthrough to the recipe if forced
  rebuilds become a common workflow.
