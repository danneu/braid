# Test: mutating-config-preflight-order
#
# What: Mutating command dispatch checks pending-op.json before loading config
# and wraps config-read failures as "config error:".
#
# Why: The Rust pool-lock boundary owns read-side fences for mutating commands;
# moving config loading into dispatch must not demote recovery guidance behind
# a bad config file.
{ braid }:
{ ... }:
{
  name = "mutating-config-preflight-order";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];
      virtualisation.memorySize = 2048;
    };

  testScript = builtins.readFile ./mutating-config-preflight-order.py;
}
