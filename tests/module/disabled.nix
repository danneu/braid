# Test: braid-module-disabled
#
# What: Imports the braid module with enable = false (the default) and no
# disks configured. The VM should boot to multi-user with no LUKS devices,
# no btrfs mount, and no /mnt/storage.
#
# Why: Proves the module is inert when disabled — importing it into a NixOS
# config doesn't break anything or add unwanted services.
#
# Dependencies: hello-world (VM infra).
{
  name = "braid-module-disabled";

  nodes.machine = { ... }: {
    imports = [ ../../modules/braid ];
  };

  testScript = builtins.readFile ./disabled.py;
}
