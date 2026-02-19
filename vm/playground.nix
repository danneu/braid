# VM: playground
#
# An interactive NixOS VM with btrfs RAID1 + Samba, accessible from the host
# Mac via SMB on localhost:4450.
#
# No LUKS — this is for playing with btrfs and SMB, not testing encryption.
#
# Usage:
#   make playground
#   # Setup runs automatically. When you see "Playground ready!", from your Mac:
#   mkdir /tmp/nas && mount_smbfs //nas@localhost:4450/storage /tmp/nas
#   # Password: nas
#
#   IMPORTANT: umount /tmp/nas BEFORE exiting the REPL, or macOS SMB hangs.
{
  name = "playground";

  nodes.nas = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    virtualisation.memorySize = 2048;

    virtualisation.forwardPorts = [
      { from = "host"; host.port = 4450; guest.port = 445; }
    ];

    environment.systemPackages = [
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
        };
      };
    };

    users.users.nas = {
      isNormalUser = true;
      description = "Samba share user";
    };

    networking.firewall.allowedTCPPorts = [ 445 ];
  };

  testScript = ''
    start_all()
    nas.wait_for_unit("multi-user.target")

    # Create btrfs RAID1 across all 3 drives
    nas.succeed(
        "mkfs.btrfs -f -d raid1 -m raid1"
        " /dev/disk/by-id/virtio-disk1"
        " /dev/disk/by-id/virtio-disk2"
        " /dev/disk/by-id/virtio-disk3"
    )
    nas.succeed("mkdir -p /mnt/storage")
    nas.succeed("mount /dev/disk/by-id/virtio-disk1 /mnt/storage")
    nas.succeed("chown nas /mnt/storage")

    # Set up Samba password (user: nas, password: nas)
    nas.succeed("(echo 'nas'; echo 'nas') | smbpasswd -a -s nas")
    nas.succeed("systemctl restart samba-smbd")
    nas.wait_for_unit("samba-smbd")

    # Verify it's working
    nas.succeed("btrfs fi show /mnt/storage")
    nas.succeed("smbclient -L localhost -N | grep -i storage")

    print("")
    print("=" * 60)
    print("  Playground ready!")
    print("")
    print("  From your Mac:")
    print("    mkdir /tmp/nas")
    print("    mount_smbfs //nas@localhost:4450/storage /tmp/nas")
    print("    Password: nas")
    print("")
    print("  To shut down:")
    print("    1. umount /tmp/nas        (Mac side FIRST)")
    print("    2. Ctrl-D or exit()       (then kill the VM)")
    print("")
    print("  Type nas.succeed('command') to run commands in the VM")
    print("=" * 60)
    print("")

    import code
    code.interact(local=locals())
  '';
}
