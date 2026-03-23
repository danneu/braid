# Test: braid-alert service with beep disabled
#
# What: Validates that braid.monitor.beep = false removes all PC speaker
# plumbing while keeping alertCommand and the alert service functional.
#
# Why: When beep is disabled, the module must not un-blacklist pcspkr, load
# the kernel module, or create the beep group. But alertCommand must still
# fire, and the alert service must still latch active for braid-ack to stop.
#
# Scenario: Headless NAS where the operator alerts via ntfy/email and does
# not want audible beeping. The alert service should run alertCommand once,
# stay active (RemainAfterExit), and leave all PC speaker config untouched.
{ braid }:
{ lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = ["disk1" "disk2"];
in
{
  name = "braid-alert-no-beep";

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
      monitor.beep = false;
      monitor.alertCommand = "touch /root/alert-fired";
    };

    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];
  };

  testScript = builtins.readFile ./braid-alert-no-beep.py;
}
