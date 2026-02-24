# Phase 7: Rust Cutover — Make Rust the Primary `braid` Binary

## Context

All 4 braid CLI commands (`init-disk`, `plan`, `apply`, `status`) are fully ported to Rust and passing both unit tests and VM integration tests (tests 15–19). The bash script `scripts/braid.sh` is now redundant for all CLI functionality. Phase 7 makes the Rust binary the primary `braid` command: rename the wrapped package, update the NixOS module, make Rust tests self-contained, convert integration tests, and retire redundant bash tests.

No new Rust code is written in this phase — it's purely packaging, test config, and module changes.

---

## 1. flake.nix packaging rename

In `craneFor`:
- Rename `braid-rust` → `braid` in the `runCommand` wrapper
- Output `$out/bin/braid` instead of `$out/bin/braid-rust`

```nix
# Before:
braid-rust = pkgs.runCommand "braid-rust" { ... } ''
  makeWrapper ${braid-cli-unwrapped}/bin/braid $out/bin/braid-rust \
    --prefix PATH : ${toolPath}
'';

# After:
braid = pkgs.runCommand "braid" { ... } ''
  makeWrapper ${braid-cli-unwrapped}/bin/braid $out/bin/braid \
    --prefix PATH : ${toolPath}
'';
```

In `packagesFor`: expose `braid` instead of `braid-rust`.

In `checksFor`: pass `braid` (and `braid-cli-unwrapped` where needed) instead of `braid-rust` to all test imports. Details in section 5 below.

---

## 2. NixOS module update

### `modules/braid/options.nix`

Rename `rustPackage` → `package`. Keep nullable for backward compat (module tests for `00-disabled` don't need to set it):

```nix
package = lib.mkOption {
  type = lib.types.nullOr lib.types.package;
  default = null;
  description = "The braid Rust CLI package (unwrapped crane output). When set, wraps and installs as 'braid'.";
};
```

### `modules/braid/cli.nix`

Replace bash braid creation with Rust-only wrapping. When `cfg.package` is set, wrap the Rust binary as `braid`. When null, skip CLI installation entirely (or assert).

```nix
{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ];

  braid = pkgs.runCommand "braid-module" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p $out/bin
    makeWrapper ${cfg.package}/bin/braid $out/bin/braid \
      --prefix PATH : ${lib.makeBinPath toolPackages}
  '';
in
{
  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = cfg.package != null;
      message = "braid.package must be set when braid.enable is true";
    }];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = cfg.disks;
      mountPoint = cfg.mountPoint;
    };

    environment.systemPackages = lib.optional (cfg.package != null) braid;
  };
}
```

Key changes:
- No more `builtins.readFile ../../scripts/braid.sh` — bash script no longer referenced
- No more `braid-rust-wrapped` — just `braid`
- Assertion ensures `package` is set when module is enabled

---

## 3. Rust VM tests — make self-contained

Tests 15, 16, 18, 19 currently use bash `braid` for setup and `braid-rust` for the tested command. After cutover, the Rust binary IS `braid`, so these tests use only the Rust binary.

### Nix config changes (all 4 tests)

- Parameter: `{ braid-rust }:` → `{ braid }:`
- Remove `braid-cli = pkgs.writeShellApplication { ... }` (bash braid)
- `environment.systemPackages`: replace `[ braid-cli braid-rust ... ]` with `[ braid ... ]`

### Python script changes (all 4 tests)

Global find-replace: `braid-rust` → `braid` in all command strings.

| File | Change |
|------|--------|
| `tests/15-braid-plan-rust.nix` | `{ braid }:`, remove bash braid-cli, use `braid` in packages |
| `tests/braid-plan-rust.py` | `braid-rust plan` → `braid plan`, `braid-rust` → `braid` everywhere |
| `tests/16-braid-apply-rust.nix` | Same pattern |
| `tests/braid-apply-rust.py` | `braid-rust apply` → `braid apply`, keep distinct `rust_apply`/`bash_apply` → just `apply` |
| `tests/18-braid-status-rust.nix` | Same pattern |
| `tests/braid-status-rust.py` | `braid-rust status` → `braid status` |
| `tests/19-braid-init-disk-rust.nix` | Same pattern |
| `tests/braid-init-disk-rust.py` | `braid-rust init-disk` → `braid init-disk` |

---

## 4. Integration tests — switch to Rust binary

These tests use `braid` by name in their Python scripts, so **Python scripts need no changes** (except test 13's warning assertion). Only the Nix configs change: accept a `{ braid }:` parameter instead of creating bash writeShellApplication.

### Test 5 (`5-braid-add-disk.nix`)

```nix
# Before: bare attrset, bash writeShellApplication inline
{ ... }

# After: accept parameter
{ braid }:
{
  name = "braid-add-disk";
  nodes.machine = { pkgs, ... }: {
    ...
    environment.systemPackages = [ braid pkgs.cryptsetup pkgs.btrfs-progs ];
    ...
  };
  ...
}
```

### Test 7 (`7-replace-failed-disk.nix`)

Same pattern: `{ braid }:` wrapper, replace bash writeShellApplication with the passed `braid` package.

### Test 9 (`9-braid-remove-disk.nix`)

Accept `{ braid }:`, replace bash `braid` with the Rust package. **Keep standalone `braid-remove-disk.sh` writeShellApplication** — that script is not ported to Rust.

### Test 12 (`12-braid-unified.nix`)

Same as test 9: accept `{ braid }:`, replace bash `braid`, keep `braid-remove-disk.sh`.

### Test 13 (`13-braid-bootstrap.nix`)

Accept `{ braid }:`, replace bash writeShellApplication with the Rust package.

**Python fix** (`tests/braid-bootstrap.py` line 56): Bash warnings are strings (`"INIT_REQUIRED: ..."`), Rust warnings are objects (`{"code": "INIT_REQUIRED", "message": "..."}`).

```python
# Before (bash format):
assert any("INIT_REQUIRED" in w for w in p["warnings"])

# After (Rust format):
assert any(w["code"] == "INIT_REQUIRED" for w in p["warnings"])
```

### Test 17 (`17-tool-versions.nix`)

Accept `{ braid, braid-cli-unwrapped }:` (rename from `{ braid-rust, braid-cli-unwrapped }:`). Update the Python script (`tests/tool-versions.py`) to reference `braid` instead of `braid-rust` in binary name checks.

---

## 5. Module tests — inject Rust binary

Module tests (01–06) import `../../modules/braid`. After the module requires `braid.package`, tests that enable the module must set it.

### Pattern for each test

```nix
# Before:
{ lib, pkgs, ... }:
{
  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];
    braid = { enable = true; disks = [ ... ]; };
    ...
  };
}

# After:
{ braid }:
{ lib, pkgs, ... }:
{
  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];
    braid = { enable = true; package = braid; disks = [ ... ]; };
    ...
  };
}
```

Tests affected:
- `tests/braid-module/01-single-disk.nix` — add `{ braid }:`, set `braid.package = braid;`
- `tests/braid-module/02-raid1.nix` — same
- `tests/braid-module/03-degraded-raid1.nix` — same
- `tests/braid-module/04-bad-config.nix` — same
- `tests/braid-module/05-single-disk-dead.nix` — same
- `tests/braid-module/06-remote-unlock.nix` — same

**Exception:** `tests/braid-module/00-disabled.nix` does NOT enable braid, so `package` is never evaluated. No change needed IF we keep the NixOS assertion inside `mkIf cfg.enable`. Verify the test still boots without setting `braid.package`.

### flake.nix check registration

Each module test gets the Rust binary passed in:

```nix
braid-module-single-disk = pkgs.testers.nixosTest (import ./tests/braid-module/01-single-disk.nix {
  braid = linuxCrane.braid-cli-unwrapped;
});
```

Note: module tests pass the **unwrapped** binary because cli.nix does its own wrapping with `cfg.packages.*`.

---

## 6. Retire redundant bash tests

Remove from `checksFor` in `flake.nix`:
- `braid-status` (test 8) — fully replaced by `braid-status-rust` (test 18)
- `braid-plan` (test 10) — fully replaced by `braid-plan-rust` (test 15)
- `braid-apply` (test 11) — fully replaced by `braid-apply-rust` (test 16)
- `braid-init-disk` (test 14) — fully replaced by `braid-init-disk-rust` (test 19)

Keep the .nix/.py files in the repo for historical reference. Just remove from `checksFor`.

---

## 7. Tests that need NO changes

These tests don't use the `braid` CLI at all (raw cryptsetup/btrfs/infrastructure tests):
- 0-hello-world, 1-luks, 2-btrfs-raid1, 3-btrfs-heal, 3-btrfs-grow, 3-btrfs-grow1, 3-btrfs-shrink, 3-btrfs-degrade
- 4-samba, 4-remote-unlock, 4-degraded-boot
- 6-first-boot-single-disk
- capture-tool-fixtures, daemon-hello-world

---

## Files modified

| File | Change |
|------|--------|
| **Packaging** | |
| `flake.nix` | Rename `braid-rust` → `braid`, update all check registrations, retire 4 bash checks, pass `braid`/`braid-cli-unwrapped` to tests |
| **Module** | |
| `modules/braid/options.nix` | `rustPackage` → `package` (nullOr package, default null) |
| `modules/braid/cli.nix` | Remove bash script, wrap Rust binary as `braid`, add assertion |
| **Rust VM tests** | |
| `tests/15-braid-plan-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-plan-rust.py` | `braid-rust` → `braid` |
| `tests/16-braid-apply-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-apply-rust.py` | `braid-rust` → `braid` |
| `tests/18-braid-status-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-status-rust.py` | `braid-rust` → `braid` |
| `tests/19-braid-init-disk-rust.nix` | `{ braid }:`, remove bash braid-cli |
| `tests/braid-init-disk-rust.py` | `braid-rust` → `braid` |
| **Integration tests** | |
| `tests/5-braid-add-disk.nix` | Accept `{ braid }:`, use Rust binary |
| `tests/7-replace-failed-disk.nix` | Accept `{ braid }:`, use Rust binary |
| `tests/9-braid-remove-disk.nix` | Accept `{ braid }:`, use Rust binary (keep standalone braid-remove-disk.sh) |
| `tests/12-braid-unified.nix` | Accept `{ braid }:`, use Rust binary (keep standalone braid-remove-disk.sh) |
| `tests/13-braid-bootstrap.nix` | Accept `{ braid }:`, use Rust binary |
| `tests/braid-bootstrap.py` | Fix warning assertion: `w["code"] == "INIT_REQUIRED"` |
| `tests/17-tool-versions.nix` | `braid-rust` → `braid` in param name |
| `tests/tool-versions.py` | `braid-rust` → `braid` in binary checks |
| **Module tests** | |
| `tests/braid-module/01-single-disk.nix` | Accept `{ braid }:`, set `braid.package` |
| `tests/braid-module/02-raid1.nix` | Same |
| `tests/braid-module/03-degraded-raid1.nix` | Same |
| `tests/braid-module/04-bad-config.nix` | Same |
| `tests/braid-module/05-single-disk-dead.nix` | Same |
| `tests/braid-module/06-remote-unlock.nix` | Same |

**NOT modified:** `scripts/braid.sh` (kept for reference), tests 0–4/6 (no braid CLI usage), `tests/braid-module/00-disabled.nix` (enable=false, no package needed).

---

## Acceptance criteria

1. `cargo test -p braid-cli` — all unit tests pass (no Rust code changes, but verify)
2. `make test-one t=braid-plan-rust` — self-contained Rust plan test passes
3. `make test-one t=braid-apply-rust` — self-contained Rust apply test passes
4. `make test-one t=braid-status-rust` — self-contained Rust status test passes
5. `make test-one t=braid-init-disk-rust` — self-contained Rust init-disk test passes
6. `make test-one t=braid-bootstrap` — Rust bootstrap integration test passes
7. `make test-one t=braid-add-disk` — Rust integration test passes
8. `make test-one t=braid-module-single-disk` — module test passes with Rust binary
9. `make test` — full suite passes (minus retired bash tests 8, 10, 11, 14)
