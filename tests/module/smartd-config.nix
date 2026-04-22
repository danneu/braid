# Test: smartd config composition
#
# Intent: Verify that braid's smartd config produces exactly one notification
#   handler, even when a sendmail wrapper is present (which causes the NixOS
#   smartd module's mail notification to default to enabled).
#
# Why it exists: The NixOS smartd module prepends its own `-m <nomailer> -M exec`
#   when any notification is enabled. Without braid explicitly disabling mail
#   notifications, installing postfix would produce duplicate directives on the
#   smartd config line — braid's script would still win (last directive wins),
#   but the NixOS notification script would be silently dropped.
#
# Scenario: NixOS machine with braid.monitor enabled and a sendmail wrapper
#   present (simulating postfix). Read the generated smartd.conf and assert it
#   contains exactly one -m and one -M exec, both from braid.
{ braid }:
{ ... }:
{
  name = "braid-smartd-config";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        monitor.enable = true;
      };

      # Simulate having a mail stack installed (e.g. postfix).
      # This makes smartd's notifications.mail.enable default to true,
      # which is the scenario braid must suppress.
      services.mail.sendmailSetuidWrapper = {
        program = "sendmail";
        source = "${pkgs.coreutils}/bin/true";
        owner = "root";
        group = "root";
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

  testScript = builtins.readFile ./smartd-config.py;
}
