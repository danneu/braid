# Test: pool-lock-precedes-state-read
#
# What: Locked commands acquire /run/braid-pool.lock before config, membership,
# journal, probe, or prompt reads.
#
# Why: Rust dispatch is the serialization boundary. State reads before that
# boundary can observe inconsistent data and hide active-operation contention.
{ braid }:
{
  name = "pool-lock-precedes-state-read";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./pool-lock-precedes-state-read.py;
}
