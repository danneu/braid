# Test: pool-lock-lock-contention
#
# What: `braid lock` fails fast when the pool operation lock is held.
#
# Why: `lock` unmounts and closes the pool, so it must be serialized with
# every in-flight pool mutation.
{ braid }:
{
  name = "pool-lock-lock-contention";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./pool-lock-lock-contention.py;
}
