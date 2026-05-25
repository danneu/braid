# Plan: Linux-only flake ergonomics (`nix run` / `nix build` defaults)

## Context

Public users can't `nix run`/`nix build` braid the easy way. The flake exposes
`packages.x86_64-linux.braid` (the wrapped CLI) but no
`packages.<system>.default` and no `apps`, so the conventional entry points
fail:

- `nix run github:danneu/braid` -> no `apps.<system>.default`, no `packages.<system>.default`
- `nix build github:danneu/braid` -> no `packages.<system>.default`

Goal: make these work on NixOS/Linux while keeping Darwin intentionally without
a runnable default (the CLI wraps Linux-only storage tooling -- cryptsetup,
btrfs-progs, systemd, NUT -- and is NixOS/Linux only).

Two independent code paths, do not conflate them:

1. **`nix run`/`nix build` want the WRAPPED `braid`.** `flake.nix:42-55` builds
   `braid` by `makeWrapper`-ing `braid-cli-unwrapped` with the storage tools on
   PATH. The runnable `default`/`apps` must resolve to this wrapped derivation.

2. **The NixOS module path is already correct and self-contained.**
   `nixosModules.default` defaults `braid.package` for the consumer:
   `flake.nix:975` sets `package = lib.mkDefault self.packages.${system}.braid-cli-unwrapped`
   (the unwrapped output, which `wrapper.nix` re-wraps with `braid.packages.*`).
   So a minimal `braid.enable = true;` config is already valid -- the module
   supplies the package.

Design-doc reconciliation: ADR `010-toolchain-pinning.md:21` ("Two wrapping
sites: flake.nix wraps with `pkgs.*` defaults (for `nix run` and tests); the
module wraps `cfg.package` with `cfg.packages.*`") names `nix run` as a wrapping
consumer. That clause is aspirational today -- `nix run` currently fails. This
change makes it functional without altering the two-wrapping-site topology
(we only expose `default`/`apps` pointing at the existing flake-wrapped `braid`).
ADR 010 stays accurate as-is; confirm, do not rewrite it.

Six end-user guide snippets set `package = braid.packages.x86_64-linux.default;`
inside a `# configuration.nix` block. These are broken and unnecessary:

- `.default` doesn't exist today (eval error), and
- even after this plan adds `.default`, the `braid` *flake input* is not in
  scope inside a `configuration.nix` module, so the line is still an
  undefined-variable error, and
- the module already defaults `braid.package` correctly, so the line shouldn't
  be there at all.

Fix: **delete** those lines (and correct the now-false "package is required"
prose). This is independent of the flake `default`/`apps` change, which only
serves `nix run`/`nix build`.

(Revised per review: the original plan rewrote the six lines to
`.braid-cli-unwrapped` and wrongly flagged the README minimal config as broken.
With `flake.nix:975` defaulting the package, removal is correct and the README
minimal config is already valid.)

## Existing structure (verified)

- `forAllSystems` lists exactly `aarch64-darwin` and `x86_64-linux`
  (`flake.nix:16-19`). x86_64-linux is the only Linux system today.
- Linux gating idiom already in use: `isLinux = builtins.match ".*-linux" system != null`
  (`flake.nix:66`, `flake.nix:993`), applied as `// (if isLinux then {...} else {})`.
- `packagesFor` (`flake.nix:62-80`): `braid-cli-unwrapped` everywhere, `braid`
  Linux-only, `playground` on aarch64-darwin. No `default`.
- No `apps` output exists. `devShells` (`flake.nix:990-999`) is the template for
  an `isLinux`-gated per-system output.
- `nixosModules.default` (`flake.nix:966-984`) defaults both `braid.package`
  (-> `braid-cli-unwrapped`) and `braid.packages.*` (-> pinned tools).

Design choice: keep the generic `isLinux` regex gate (matching existing style)
rather than hardcoding `x86_64-linux`. Functionally x86_64-linux-only today, but
a future `aarch64-linux` entry in `forAllSystems` would light up
`default`/`apps` automatically. Not adding `aarch64-linux` now (multiplies the
VM test matrix -- out of scope).

## Changes

### 1. `flake.nix` -- add `packages.<linux>.default` (wrapped)

Replace the one-line Linux gate (`flake.nix:72`) so it also exposes `default`,
binding `braid` once:

```nix
        // (
          if isLinux then
            let
              inherit (craneFor system) braid;
            in
            {
              inherit braid;
              default = braid;
            }
          else
            { }
        )
```

Result on x86_64-linux: `packages.x86_64-linux.{braid, default, braid-cli-unwrapped, nix-fast-build}`.
Darwin unchanged (no `braid`, no `default`).

### 2. `flake.nix` -- add `apps` (Linux-only), after `packages =` (`flake.nix:986`)

```nix
      # Linux-only runnable CLI. `nix run`/`nix build` resolve `default` (and the
      # explicit `braid` attr) to the wrapped binary, which carries the storage
      # tooling (cryptsetup, btrfs-progs, systemd, ...) on PATH. Darwin gets no
      # runnable app on purpose -- the CLI targets NixOS/Linux only.
      apps = forAllSystems (
        system:
        let
          isLinux = builtins.match ".*-linux" system != null;
        in
        if isLinux then
          let
            braidApp = {
              type = "app";
              program = "${self.packages.${system}.braid}/bin/braid";
            };
          in
          {
            braid = braidApp;
            default = braidApp;
          }
        else
          { }
      );
```

`self.packages.${system}.braid` keeps the app and `nix build` default the
identical store path; `${...}/bin/braid` matches the wrapper output layout
(`flake.nix:52-53`). On Darwin `apps.aarch64-darwin = {}` (intentionally empty).

### 3. `README.md` -- public run note (`## Install`, around `README.md:49-51`)

Insert a platform note + zero-install run example before "Add braid to your
flake inputs ...". Cookbook style, ASCII `--`:

```markdown
## Install

> NixOS/Linux only (x86_64). The CLI wraps Linux storage tooling (LUKS, btrfs,
> systemd) and does not run on macOS.

Try it without installing anything:

```sh
nix run github:danneu/braid -- --help
```

Add braid to your flake inputs and import the module:
```

Leave the existing module-install block and the minimal `configuration.nix`
(`README.md:74-80`, which correctly omits `package`) intact.

### 4. Delete the 6 broken `package = ...` lines and fix the false prose

Straight removal of the `package = braid.packages.x86_64-linux.default;` line
(the module defaults it):

- `docs/guides/getting-started.md:51`
- `docs/guides/nixos-configuration.md:34`
- `docs/guides/auto-unlock.md:75`
- `docs/guides/monitoring-and-alerts.md:41`
- `docs/guides/power-management.md:38`

For the "Full config example" (`docs/guides/nixos-configuration.md:166`), which
documents "every option," replace the active line with a comment matching the
already-commented `packages.*` overrides just below it, e.g.
`# package -- defaults to nixosModules.default's pinned braid-cli-unwrapped; set only to build the CLI yourself`.

Fix the now-false "required" prose:

- `getting-started.md:56` "`braid.enable` and `braid.package` are required." ->
  only `braid.enable = true` is required; `nixosModules.default` defaults
  `braid.package` to the pinned `braid-cli-unwrapped`.
- `nixos-configuration.md:38` "`braid.package` is required ... will fail
  evaluation without it." -> module supplies the default; override only to build
  the CLI yourself.
- `nixos-configuration.md:60` table row "The braid CLI package (required when
  enabled)" -> note `nixosModules.default` defaults it to `braid-cli-unwrapped`
  (the bare option default is still `null`, accurate to `options.nix:31`).

### 5. Add a committed eval check for the `nixosModules.default` package default

Deleting the explicit `package =` line from every guide makes "`nixosModules.default`
supplies `braid.package`" the *sole* documented install path -- yet nothing
tests it. `tests/eval/_braid-eval-harness.nix:5` imports `../../modules/braid`
directly and `:16` passes an explicit `package = linuxPkgs.writeShellScriptBin ...`,
and `rg "nixosModules\.default" tests/` is empty. So the flake wrapper's
`package = lib.mkDefault self.packages.<sys>.braid-cli-unwrapped` (`flake.nix:975`)
is exercised by nothing; a rename/refactor that drops it would silently break
every documented install.

Add a pure-eval check modeled on the existing `tests/eval/` entries
(`flake.nix:800-807`). New file `tests/eval/nixos-module-default-package.nix`
(with the standard Intent/Why/Scenario preamble per Test Conventions):

```nix
{ pkgs, self, nixpkgs, linuxSystem }:
let
  sys = nixpkgs.lib.nixosSystem {
    system = linuxSystem;
    modules = [
      self.nixosModules.default
      { braid.enable = true; }
    ];
  };
in
# Force ONLY config.braid.package -- never config.system.build.toplevel/
# config.assertions -- so the check stays pure-eval and does not compile the real
# braid-cli-unwrapped crane derivation. If the flake.nix:975 mkDefault is dropped,
# braid.package falls back to its null option default and THIS file's own `assert`
# throws at eval; the module's options.nix:92 assertion is intentionally never the
# firing path here. (This guards "the default yields a non-null package," not "the
# options.nix:92 assertion exists.")
assert sys.config.braid.package != null;
pkgs.runCommand "eval-nixos-module-default-supplies-package" { } ''
  touch $out
''
```

Register it in `checksFor` next to the existing `eval-*` entries (`self` is in
scope via the `outputs` closure):

```nix
          eval-nixos-module-default-supplies-package =
            import ./tests/eval/nixos-module-default-package.nix {
              inherit pkgs self nixpkgs linuxSystem;
            };
```

Properties: it goes through `self.nixosModules.default` (the untested wrapper),
not `./modules/braid`; it asserts only the package option, so no
bootloader/root-fs harness is needed and no Rust build is triggered; and because
the `runCommand` has no Linux build inputs, the check builds *natively* on both
`checks.aarch64-darwin` and `checks.x86_64-linux` (the cross-system crane
derivation is evaluated to WHNF, not built). Name is `eval-`-prefixed, so it
lands in the regular `checks` set (not `repro-`).

## Verification

### Cheap eval checks (run from repo root on this Darwin host; no linux-builder)

Evaluation of x86_64-linux derivation *metadata* works on Darwin; only building
needs the linux-builder. The module-eval snippets below use a `git+file://`
flakeref so only git-tracked files are read -- a bare path flake
(`getFlake (toString ./.)`) would copy `cli/target` and other gitignored build
output into the store. `git+file://` includes dirty edits to tracked files;
`git add` any newly created files first.

New flake outputs:

- `nix flake show --all-systems` -- expect
  `packages.x86_64-linux.{braid, braid-cli-unwrapped, default, nix-fast-build}`
  and `apps.x86_64-linux.{braid, default}`; expect NO
  `packages.aarch64-darwin.default` and NO `apps.aarch64-darwin.*`.
  (`--all-systems` because plain `nix flake show` on Darwin can omit
  non-current-system detail.)
- `nix eval .#packages.x86_64-linux.default.name` -- resolves (wrapped `braid`).
- `nix eval .#apps.x86_64-linux.default.program` -- prints `/nix/store/.../bin/braid`.
- `nix eval .#apps.x86_64-linux.default.type --raw` -- prints `app`.
- Intentional Darwin absence (each must error "attribute ... missing"):
  `nix eval .#packages.aarch64-darwin.default` and
  `nix eval .#apps.aarch64-darwin.default`.
- `nix build .` on Darwin -- expect failure: `packages.aarch64-darwin.default`
  does not exist (intended; no Darwin default).

Module install snippet (proves the docs' minimal config actually evaluates --
the behavior the doc edits now claim). Cheap check -- evaluating one option
value does not force `toplevel`, so no bootloader/root-fs options are needed:

```sh
nix eval --impure --expr '
  let f = builtins.getFlake "git+file://${toString ./.}";
  in (f.inputs.nixpkgs.lib.nixosSystem {
       system = "x86_64-linux";
       modules = [ f.nixosModules.default { braid.enable = true; } ];
     }).config.braid.package.name
'
```

- Expect a non-null name (e.g. `braid-cli-0.0.1`) -- proves `nixosModules.default`
  supplies `braid.package`, so the assertion `cfg.package != null`
  (`options.nix:92`) passes by construction.

Fuller check (instantiate-only, no build) that forces the whole minimal system
and every braid assertion. Forcing `toplevel` pulls in NixOS's generic
bootloader/root-fs assertions, so add a minimal harness:

```sh
nix eval --impure --expr '
  let f = builtins.getFlake "git+file://${toString ./.}";
  in (f.inputs.nixpkgs.lib.nixosSystem {
       system = "x86_64-linux";
       modules = [
         f.nixosModules.default
         {
           braid.enable = true;
           boot.loader.grub.devices = [ "nodev" ];
           fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
           system.stateVersion = "25.11";
         }
       ];
     }).config.system.build.toplevel.drvPath
'
```

- Expect a `.drv` path and NO `braid.package must be set` assertion error.

Committed check (Change 5) -- builds natively on this Darwin host, no
linux-builder:

- `nix build .#checks.aarch64-darwin.eval-nixos-module-default-supplies-package`
  -- succeeds (assertion evaluated, trivial `touch $out` built). To prove it
  actually guards: temporarily edit `flake.nix:975` to `package = lib.mkDefault null;`
  and confirm the build now fails at eval with the check's own
  `assertion '... != null' failed` (config.braid.package resolved to null) -- not
  the `options.nix:92` message, which this pure-eval check never forces -- then revert.

### Build/run checks (NixOS/Linux x86_64 host, or via linux-builder from Darwin)

From Darwin (linux-builder, explicit system selectors):

- `nix build .#packages.x86_64-linux.default --no-link --print-out-paths`
- `nix build .#packages.x86_64-linux.braid --no-link --print-out-paths`
  (same wrapped store path expected)

On a NixOS/Linux x86_64 host (the public path):

- `nix build .` and `nix build .#braid` -- produce `result/bin/braid`.
- `nix run . -- --help` and `nix run .#braid -- --help` -- print braid help.
- After merge+push: `nix run github:danneu/braid -- --help` and
  `nix run github:danneu/braid#braid -- --help`.

### Doc consistency

- `rg -n "packages\.x86_64-linux\.default" docs README.md` -- no remaining
  module-`package` refs (README's only `default`-adjacent mention is the
  `nix run` example, which uses `nix run`, not the attr).
- `rg -n "braid\.package.*required|required.*braid\.package" docs` -- no
  remaining "package is required" prose.

## Tests / fixtures

- One new pure-eval check (Change 5), no new VM test. The flake output additions
  are plumbing and the wrapped derivation is already built by crane; the new
  `eval-nixos-module-default-supplies-package` check is the only behavioral
  regression guard added, covering the now-sole documented install path.
- No fixture refresh. No parser-critical tool version (`btrfs-progs`,
  `cryptsetup`, `util-linux`, `nut`, `smartmontools`) or `nixpkgs` pin changes.
- `nix flake check` (full VM suite, 20-30 min) is NOT required; defer to the
  user's normal full-suite run.

## Risks

- **aarch64-linux not exposed.** `nix run github:danneu/braid` on an aarch64
  Linux host still fails -- `forAllSystems` lists only `x86_64-linux` (+
  aarch64-darwin). Accepted: NAS targets are x86_64, and the generic `isLinux`
  gate makes future expansion a one-line `forAllSystems` edit. Out of scope.
- **No double-wrap interaction.** The flake `default`/`apps` (wrapped) and the
  module `package` default (unwrapped, via `nixosModules.default`) are fully
  independent. Deleting the 6 doc lines leaves the module on its own correct
  default; nothing feeds the wrapped pkg into the module's re-wrapper.
- **`self` reference in `apps`.** Standard flake idiom; lazy, no recursion.

## Out of scope

Release packaging, CI workflow/pipeline rewrite (Change 5 adds one check via the
existing `checks` set -- not a pipeline change), Darwin `devShells.default`,
adding `aarch64-linux`, and changing the module wrapping design.

## Implementation notes

- `tests/eval/nixos-module-default-package.nix` uses a `#` Intent/Why/Scenario preamble because Nix does not have `//` line comments; using the repo's literal Rust/Python preamble form would make the Nix file invalid.
- The eval check targets `x86_64-linux` explicitly instead of `checksFor`'s `linuxSystem` helper because Darwin checks map that helper to `aarch64-linux`, which this flake intentionally does not expose today.
