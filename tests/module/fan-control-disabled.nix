# Test: fan-control-disabled
#
# Intent: Verify that /etc/braid/config.json omits the `fan_control` key
# entirely when braid.fanControl.enable = false.
#
# Why it exists: the braid CLI uses the absence of `fan_control` in
# config.json as its "fan telemetry disabled" signal. If cli.nix ever
# started emitting a default-zeroed `fan_control` block, the TUI would
# render a Fans section with garbage values for users who never opted in.
# This test pins the opt-in contract: enable=false means the key is not
# written.
#
# Scenario: NixOS VM with braid enabled but braid.fanControl.enable = false.
# Inspects the generated /etc/braid/config.json at boot and asserts
# `fan_control` is absent.
{ braid }:
{
  name = "fan-control-disabled";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        # fanControl.enable default is false; leaving it out makes the
        # intent explicit.
      };
    };

  testScript = builtins.readFile ./fan-control-disabled.py;
}
