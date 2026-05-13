# Test: braid-lock-name-order
#
# What: Runs `braid lock` against a hand-crafted pool.json whose UUID order is
# opposite name order.
#
# Why: The "already closed" prelude is operator-facing and must not inherit
# UUID-keyed persistence order.
#
# Dependencies: Rust braid binary and a minimal generated config.
{ braid }:
{
  name = "braid-lock-name-order";

  nodes.machine =
    { ... }:
    {
      environment.systemPackages = [ braid ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-lock-name-order.py;
}
