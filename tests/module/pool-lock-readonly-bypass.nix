# Test: pool-lock-readonly-bypass
#
# What: Read-only operator diagnostics stay available while
# /run/braid-pool.lock is held by another process.
#
# Why: `status` and `doctor` are the commands operators need during an
# incident; they must not join the mutating pool-operation critical section.
{ braid }:
{
  name = "pool-lock-readonly-bypass";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./pool-lock-readonly-bypass.py;
}
