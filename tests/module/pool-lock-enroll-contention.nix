# Test: pool-lock-enroll-contention
#
# What: `braid enroll` fails fast when the pool operation lock is held.
#
# Why: LUKS keyslot mutation must be serialized with pool topology mutation
# and recovery.
{ braid }:
{
  name = "pool-lock-enroll-contention";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./pool-lock-enroll-contention.py;
}
