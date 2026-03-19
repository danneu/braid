# Test: braid-alert service lifecycle
#
# What: Validates that the braid monitor timer and alert service exist and
# can be started/stopped when `braid.monitor.enable` is true.
#
# Why: The monitor timer and alert service are the systemd plumbing that
# makes alerting work. If they fail to activate or the units are missing,
# disk health issues will go undetected.
#
# Scenario: NixOS machine with braid.monitor enabled. Verify the timer is
# active, the alert service unit exists, and it can be started and stopped.
{ braid }:
{ lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = ["disk1" "disk2"];
in
{
  name = "braid-alert";

  nodes.machine = { pkgs, ... }: {
    imports = [
      ../../modules/braid
      (import ./lib/initrd-fixture.nix { inherit passphrase diskNames; })
    ];

    braid = {
      enable = true;
      package = braid;
      disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
      monitor.enable = true;
    };

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];
  };

  testScript = builtins.readFile ./braid-alert.py;
}
