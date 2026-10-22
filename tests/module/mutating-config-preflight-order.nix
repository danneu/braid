# Test: mutating-config-preflight-order
#
# What: Dispatch checks pending-op.json before loading config or pool.json for
# every command recovery mode refuses, and the add, remove, remove-missing and
# replace arms wrap config-read failures as "config error:".
#
# Why: The Rust pool-lock boundary owns read-side fences for mutating commands;
# loading config or pool.json in dispatch must not demote recovery guidance
# behind a bad config file or a missing or corrupt pool.json.
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
