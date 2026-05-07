# Test: ups-credential-lifecycle
#
# What: Boots the VM with braid.ups.enable = true and the shared dummy-ups
# fixture, then audits the runtime-generated upsmon credential across the
# places it must and must not appear.
#
# Why: decision 020's "never enters the Nix store" claim is load-bearing for
# braid's UPS integration. The credential must stay a runtime secret consumed
# through nixpkgs `power.ups` passwordFile paths, not a value embedded into
# declarative outputs, process metadata, or logs.
{ braid }:
{ pkgs, lib, ... }:
{
  name = "ups-credential-lifecycle";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/ups-fixture.nix { })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./ups-credential-lifecycle.py;
}
