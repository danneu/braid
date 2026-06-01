# Test: braid-auto-suspend module configuration
#
# What: Validates that enabling braid.autoSuspend produces the correct autosuspend
# configuration with the BraidPool check, BraidWol check, SSH check, Smb
# auto-detection, and scrub wakeup.
#
# Why: The autosuspend integration is the wiring between braid's idle check
# and the system suspend daemon. If a check command is wrong or the service
# fails to start, the NAS will either never suspend, suspend during operations,
# or suspend without a verified wake path.
#
# Scenario: Enable braid.autoSuspend + samba. Verify autosuspend service is
# configured, all expected checks exist, and the BraidPool/BraidWol commands
# use fully qualified store paths.
{ braid }:
{ ... }:
{
  name = "braid-auto-suspend";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        autoSuspend.enable = true;
        autoSuspend.wolInterface = "eth0";
        packages.ethtool = pkgs.writeShellScriptBin "ethtool" ''
          if [ "$#" -ne 1 ]; then
            echo "fake ethtool expects exactly one interface" >&2
            exit 64
          fi

          mode=g
          if [ -r /tmp/braid-wol-mode ]; then
            read -r mode < /tmp/braid-wol-mode || mode=g
          fi

          printf 'Settings for %s:\n\tSupports Wake-on: pumbg\n\tWake-on: %s\n' "$1" "$mode"
        '';
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
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
    };

  testScript = builtins.readFile ./braid-auto-suspend.py;
}
