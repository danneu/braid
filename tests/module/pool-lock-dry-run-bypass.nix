# Test: pool-lock-dry-run-bypass
#
# What: --dry-run mutators must not acquire /run/braid-pool.lock.
#
# Why: `lock_policy` returns None for every dry-run mutator, so dispatch must
# pass through `acquire_per_policy` without acquiring. Pins the runtime side of
# the classification unit test.
{ braid }:
{
  name = "pool-lock-dry-run-bypass";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./pool-lock-dry-run-bypass.py;
}
