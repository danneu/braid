# Test: braid-module-invalid-disk-keys
#
# What: Evaluates the braid module with invalid and valid disk keys to verify
# that assertion validation fires correctly. This is an eval-time test — no VM
# is booted.
#
# Why: Disk keys become device-mapper names (braid-<key>), which flow into
# systemd unit names, initrd scripts, and kernel interfaces. Invalid characters
# could cause silent breakage. This validates the Nix-side guard.
#
# Dependencies: None (pure eval-time check).
{ nixpkgs, system }:
let
  pkgs = nixpkgs.legacyPackages.${system};
  lib = pkgs.lib;

  # Evaluate options.nix with given disk config, return assertions list
  evalBraid = disks:
    (lib.evalModules {
      modules = [
        ../../modules/braid/options.nix
        { _module.args = { inherit pkgs; }; }
        {
          options.assertions = lib.mkOption {
            type = lib.types.listOf lib.types.unspecified;
            default = [];
          };
        }
        {
          braid = {
            enable = true;
            inherit disks;
          };
        }
      ];
    }).config.assertions;

  # Returns true if evaluating with these disks triggers a disk-key assertion failure
  hasDiskKeyError = disks:
    let
      assertions = evalBraid disks;
      failed = builtins.filter (a: !a.assertion) assertions;
    in
    builtins.any (a: lib.hasInfix "invalid disk key" a.message) failed;

  checkReject = name: disks:
    if hasDiskKeyError disks
    then "echo 'PASS: ${name} correctly rejected'"
    else "echo 'FAIL: ${name} should have been rejected' && exit 1";

  checkAccept = name: disks:
    if !(hasDiskKeyError disks)
    then "echo 'PASS: ${name} correctly accepted'"
    else "echo 'FAIL: ${name} should have been accepted' && exit 1";
in
pkgs.runCommand "braid-module-invalid-disk-keys" {} ''
  ${checkReject "1startsWithDigit" { "1startsWithDigit" = { byId = "/dev/disk/by-id/a"; }; }}
  ${checkReject "-startsWithHyphen" { "-startsWithHyphen" = { byId = "/dev/disk/by-id/b"; }; }}
  ${checkReject "_startsWithUnderscore" { "_bad" = { byId = "/dev/disk/by-id/c"; }; }}
  ${checkReject "tooLong33chars" { "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" = { byId = "/dev/disk/by-id/d"; }; }}
  ${checkAccept "exactly32chars" { "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" = { byId = "/dev/disk/by-id/e"; }; }}
  ${checkAccept "valid-names" {
    toshiba = { byId = "/dev/disk/by-id/a"; };
    disk1 = { byId = "/dev/disk/by-id/b"; };
    my-disk = { byId = "/dev/disk/by-id/c"; };
    my_disk = { byId = "/dev/disk/by-id/d"; };
    A = { byId = "/dev/disk/by-id/e"; };
  }}
  touch $out
''
