# Pivot: fix NUT packaging, then split `braid ups status` invocation failures

## Context

`upsc` should be present for supported braid installs. NUT is already a
parser-critical tool in `docs/decisions/010-toolchain-pinning.md` and
`docs/principles.md`, and the NixOS module path is mostly wired that
way:

- `modules/braid/wrapper.nix` already puts `cfg.packages.nut` on the
  deployed `braid` wrapper `PATH`.
- `modules/braid/ups.nix` already sets `power.ups.package =
  cfg.packages.nut` when `braid.ups.enable = true`.

There are still two packaging/pinning gaps:

- The top-level flake package wrapper (`packages.<system>.braid`) omits
  `pkgs.nut` from its `toolPath`, even though `braid ups status` shells
  out to `upsc`.
- `nixosModules.default` pins the other parser-critical package options
  to braid's nixpkgs (`cryptsetup`, `btrfsProgs`, `utilLinux`,
  `smartmontools`) but omits `nut`, so `braid.packages.nut` falls back
  to the consumer module `pkgs.nut` default from `options.nix`.

Fix those first. After that, missing `upsc` becomes an invariant-break
path: direct use of `braid-cli-unwrapped`, a broken wrapper, a bad
package override, or a runner-level failure such as a signal-killed
child. The existing human-mode `braid ups status` fallback should still
be truthful for those cases.

Today the typed layer already distinguishes runner-level failure from
non-zero `upsc` exit:

- `UpsQueryError::InvocationFailed` means `upsc` could not be invoked.
- `UpsQueryError::QueryFailed` means `upsc` ran and returned non-zero.

`--json`, `doctor`, and mutation preflight already honor that split. The
plain human `braid ups status` surface is the outlier: it wraps
invocation failures in `UpsError::QueryFailed`, producing:

```text
error: upsc query failed: invocation failed: command failed: upsc ups: No such file or directory (os error 2)
```

(`upsc ups` is the real command string because `CmdRequest::UpscQuery`
renders `program=upsc args=[name]` and `RealRunner` joins them with one
space.)

## Approach

Make the packaging invariant true, prove it, then keep the error split
as a diagnostic fallback.

### Packaging and pinning

1. Add `pkgs.nut` to the top-level flake package wrapper `toolPath` in
   `flake.nix`, beside the other parser-critical runtime tools. This
   fixes `nix run .#braid` / `packages.braid` for `braid ups status`.

2. Add `nut = lib.mkDefault braidPkgs.nut;` under
   `nixosModules.default.config.braid.packages` in `flake.nix`. This
   makes `braid.packages.nut` match the existing decision-doc claim:
   NUT defaults to braid's pinned nixpkgs input, while remaining
   overrideable by operators.

3. Strengthen `tests/cli/tool-versions` so the gap stays closed:

   - Change the Nix test import to take both
     `braid-cli-unwrapped = linuxCrane.braid-cli-unwrapped` and
     `braidWrappedPackage = linuxCrane.braid`. Continue setting
     `braid.package = braid-cli-unwrapped` so the VM still exercises the
     module wrapper, and write
     `environment.etc."braid/top-level-braid-path".text =
     "${braidWrappedPackage}/bin/braid\n";` so the Python test can also
     execute the top-level package wrapper by absolute path.
   - Add `pkgs.nut` to the VM's expected-version JSON and, if needed
     for direct shell provenance checks, to `environment.systemPackages`.
   - Assert `upsc` resolves to a `/nix/store/` path and `upsc -V` starts
     with `Network UPS Tools upsc ${expected["nut"]}`.
   - Add wrapper-behavior checks that run both the module wrapper and
     the top-level wrapped package with `PATH=/nonexistent` and a temp
     config containing `ups.name = "ups"`. With no `upsd` configured in
     this VM, success means the command exits non-zero with JSON
     `error == "query_failed"` and empty stderr. If NUT is missing from
     either wrapper, the same command reports `invocation_failed`, so the
     test catches the exact packaging regression.

   The temp config only needs the deserialized CLI fields:

   ```json
   {
     "mount_point": "/mnt/storage",
     "pool_access_group": "storage",
     "systemd_lifecycle": true,
     "ups": { "name": "ups" }
   }
   ```

### `braid ups status` fallback wording

4. Add `UpsError::InvocationFailed { detail: String }` in
   `cli/src/ups.rs`. Keep `QueryFailed` for non-zero `upsc` exits.
   Human display becomes:

   ```text
   upsc invocation failed: <CmdError> -- is pkgs.nut on PATH?
   ```

5. Rewrite `emit_invocation_failed` to take the wrapped `CmdError`
   directly. For JSON, emit the bare `CmdError` display under the
   existing `invocation_failed` sentinel. For human mode, return
   `UpsError::InvocationFailed` with the same bare detail plus the PATH
   hint. Reuse `QueryFailedJsonReported` for the JSON arm so `main.rs`
   continues to exit 1 without printing duplicate stderr.

6. Update the `cmd_ups_status` call site to pass the `CmdError`
   directly instead of building `format!("invocation failed: {e}")`.
   Leave `UpsQueryError`, `doctor`, preflight, TUI, and `main.rs`
   matching logic unchanged.

7. Update the UPS status docs tables in `manual/commands/ups-status.md`
   and `manual/guides/ups.md` so the `invocation_failed` JSON example
   uses the bare production-shaped detail:

   ```json
   {"error": "invocation_failed", "detail": "command failed: upsc ups: No such file or directory (os error 2)"}
   ```

   Keep the `query_failed` and `ups_not_enabled` rows unchanged.

### Tests for fallback wording

8. Update `cli/src/ups.rs` tests:

   - `cmd_ups_status_invocation_failure_surfaces_typed_error` should
     expect `UpsError::InvocationFailed`, require the PATH hint, reject
     the legacy `"invocation failed"` detail prefix, and reject
     `"query failed"` in the full display.
   - `json_invocation_failed_has_sentinel_error_and_detail` and
     `snapshot_json_invocation_failed` should use a production-shaped
     synthetic detail like `command failed: upsc ups: No such file or
     directory`, and assert the detail contains `command failed: upsc `
     rather than the old prefix.
   - Keep `cmd_ups_status_invocation_failure_json_returns_already_reported`
     unchanged.

9. Update `tests/cli/braid-status-ups.py`:

   - Keep the existing unwrapped-binary `PATH=/nonexistent` JSON test,
     because it intentionally simulates the invariant-break path.
   - Tighten the JSON detail assertion to require
     `detail_if.startswith("command failed: upsc ")` and
     `"invocation failed" not in detail_if`.
   - Add the parallel human-mode assertion for the unwrapped binary:
     stderr starts with `error: upsc invocation failed:`, contains
     `-- is pkgs.nut on PATH?`, and does not contain
     `upsc query failed`.

## Critical files

- `flake.nix` -- add NUT to the top-level wrapper and the module default
  pinned package set; pass the top-level wrapped package into the
  tool-version VM test.
- `tests/cli/tool-versions.nix` and `tests/cli/tool-versions.py` -- add
  NUT version/provenance coverage and wrapper behavior checks for both
  package paths.
- `cli/src/ups.rs` and `cli/src/snapshots/snapshot_json_invocation_failed.snap`
  -- split human invocation failure from query failure and refresh the
  JSON snapshot.
- `manual/commands/ups-status.md`, `manual/guides/ups.md`, and
  `tests/cli/braid-status-ups.py` -- update the documented JSON detail
  and live invocation-failure assertions.

No docs decision change is needed: decision 010 and principle 10 already
say NUT is parser-critical and pinned. This plan makes the implementation
match those existing docs.

## Verification

1. `just test-vm tool-versions` -- proves `upsc` is pinned, resolves
   from `/nix/store/`, and both the module wrapper and top-level package
   wrapper find `upsc` even with `PATH=/nonexistent`.
2. `just test-rust` -- updated `ups.rs` unit tests pass. Run
   `cargo insta review` or `cargo insta accept` once to refresh only
   `snapshot_json_invocation_failed.snap`.
3. `just test-vm braid-status-ups` -- live NUT canary still covers
   success, query failure, not-enabled, and the intentional unwrapped
   invocation-failure fallback.
4. Optional manual sanity in a VM:

   ```sh
   PATH=/nonexistent braid --config /tmp/ups-config.json ups status --json
   # stdout: {"error":"query_failed", ...}
   # proves the wrapper found upsc; upsd is simply unavailable

   PATH=/nonexistent /nix/store/.../braid-cli/bin/braid ups status
   # stderr: error: upsc invocation failed: command failed: upsc ups: ... -- is pkgs.nut on PATH?
   # proves the unwrapped invariant-break path remains truthful
   ```

## Implementation notes

- `tests/cli/braid-status-ups.py` now unwraps through the module wrapper
  and the top-level package wrapper to find the `braid-cli` store path;
  the top-level package wrapper correctly carries NUT now, so using it as
  the simulated invariant-break binary would report `query_failed`.
