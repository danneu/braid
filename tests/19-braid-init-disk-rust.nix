# Test: braid init-disk (Rust implementation)
#
# What: Exercises the Rust braid init-disk command — safety checks, LUKS
# formatting, passphrase validation, and force/confirmation gates.
#
# Why: init-disk is the ONLY path that may call cryptsetup luksFormat.
# All safety checks must hold: block-device check, declared-disk requirement,
# pool-membership refusal, isLuks refusal, force-confirmation, passphrase
# check, format execution. This mirrors the bash test (14-braid-init-disk)
# against the Rust implementation.
#
# Dependencies: LUKS primitives (cryptsetup), btrfs basics, bash braid for
# pool setup.
{ braid-rust }:
{
  name = "braid-init-disk-rust";

  nodes.machine = { pkgs, ... }: let
    braid-cli = pkgs.writeShellApplication {
      name = "braid";
      runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
      text = builtins.readFile ../scripts/braid.sh;
    };
  in {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      braid-cli
      braid-rust
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];
  };

  testScript = builtins.readFile ./braid-init-disk-rust.py;
}
