# Test: braid-auto-suspend module configuration
#
# What: Validates that enabling braid.autoSuspend produces the correct autosuspend
# configuration with the BraidPool check, SSH check, Smb auto-detection,
# and scrub wakeup.
#
# Why: The autosuspend integration is the wiring between braid's idle check
# and the system suspend daemon. If the check command is wrong or the service
# fails to start, the NAS will either never suspend or suspend during operations.
#
# Scenario: Enable braid.autoSuspend + samba. Verify autosuspend service is
# configured, all expected checks exist, and the BraidPool command uses
# fully qualified store paths.
{ braid }:
{ ... }:
{
  name = "braid-auto-suspend";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    braid = {
      enable = true;
      package = braid;
      autoSuspend.enable = true;
      autoSuspend.wolInterface = "eth0";
    };

    services.samba = {
      enable = true;
      settings.storage = {
        path = "/mnt/storage";
        browseable = "yes";
        "read only" = "no";
        "guest ok" = "yes";
      };
    };

    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];
  };

  testScript = builtins.readFile ./braid-auto-suspend.py;
}
