# Test: braid-discover-empty-scan
#
# What: Boots a minimal diskless braid node (no disks, no LUKS fixture) and
# drives bare `braid discover` and `braid discover --write`. With zero
# braid-labeled LUKS2 disks attached, both must exit non-zero with the
# no-members refusal on stderr, print no preview rows on stdout, and `--write`
# must not create pool.json.
#
# Why: the empty-scan refusal is the only discover refusal with no end-to-end
# coverage -- braid-discover.py always boots two labeled disks, so it can never
# reach members.is_empty(). The unit test only pins the message string and the
# pool-lock sentinel only asserts the string's absence under contention.
{ braid }:
{
  name = "braid-discover-empty-scan";

  nodes.machine =
    { ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };
    };

  testScript = builtins.readFile ./braid-discover-empty-scan.py;
}
