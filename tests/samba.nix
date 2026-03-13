# Test: samba
#
# What: Creates a btrfs RAID1 on LUKS devices on the server, exports it via
# Samba, and a client VM mounts the share over SMB and does a write/read
# round-trip.
#
# Why: This is the user-facing access path. The NAS exists to serve files
# over SMB to macOS/Windows/Linux clients. If the client can't mount and
# read/write, nothing else matters.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 work).
{
  name = "samba";

  nodes.server = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];

    services.samba = {
      enable = true;
      settings = {
        storage = {
          path = "/mnt/storage";
          browseable = "yes";
          "read only" = "no";
          "guest ok" = "no";
          "force user" = "nas";
          "force group" = "storage";
        };
      };
    };

    users.groups.storage = {};

    users.users.nas = {
      isNormalUser = true;
      description = "Samba share user";
      extraGroups = [ "storage" ];
    };

    networking.firewall.allowedTCPPorts = [ 445 ];
  };

  nodes.client = { pkgs, ... }: {
    environment.systemPackages = [ pkgs.cifs-utils ];
  };

  testScript = builtins.readFile ./samba.py;
}
