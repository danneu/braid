# Test: btrnas-module-disabled
#
# What: Imports the btrnas module with enable = false (the default) and no
# disks configured. The VM should boot to multi-user with no LUKS devices,
# no btrfs mount, and no /mnt/storage.
#
# Why: Proves the module is inert when disabled — importing it into a NixOS
# config doesn't break anything or add unwanted services.
#
# Dependencies: hello-world (VM infra).
{
  name = "btrnas-module-disabled";

  nodes.machine = { ... }: {
    imports = [ ../../modules/btrnas ];
  };

  testScript = builtins.readFile ./00-disabled.py;
}
