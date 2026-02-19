# Test: remote-unlock
#
# What: Server boots into initrd with 3 LUKS-encrypted drives pre-formatted
# as btrfs RAID1. Initrd exposes SSH on :2222 and blocks waiting for LUKS
# passphrases. Client SSHs in, unlocks all 3 drives, and the server continues
# to full boot with the btrfs pool mounted.
#
# Why: This is the boot-time experience. The NAS powers on, you SSH in from
# your laptop, type the passphrase, and the NAS comes up. Without this, you'd
# need a keyboard and monitor attached to unlock.
#
# Dependencies: btrfs-raid1 (LUKS + btrfs RAID1 work), hello-world (VM infra).
#
# Pattern reference:
# - nixpkgs/nixos/tests/systemd-initrd-networkd-ssh.nix
# - nixpkgs/nixos/tests/systemd-initrd-luks-password.nix
#
# Hard-won design notes (each of these caused test failures):
#
# 1. The btrfs mount MUST happen in initrd (neededForBoot=true), not stage 2.
#    LUKS mapper devices (/dev/mapper/disk*) don't survive switch-root unless
#    something in initrd holds a reference to them. Without a neededForBoot
#    mount, /dev/mapper/ is empty after switch-root.
#
# 2. btrfs RAID1 needs ALL member devices present to mount successfully.
#    But LUKS devices are unlocked sequentially over SSH. If the mount unit
#    triggers as soon as the first device appears (systemd's default behavior),
#    btrfs fails with "failed to read chunk root" because the other devices
#    aren't open yet. The mount uses x-systemd.requires/after to wait for a
#    btrfs-device-scan service, which itself waits for all cryptsetup units.
#
# 3. The fixture MUST use -fmt suffix mapper names (disk1-fmt, not disk1).
#    If the fixture opens devices with the real names, /dev/mapper/disk1
#    appears and systemd immediately triggers the mount unit — before btrfs
#    is even formatted. This causes a kernel panic via initrd-fs.target
#    failure and panic-on-fail.
#
# 4. btrfs records device paths at mkfs time. Since the fixture used -fmt
#    names, the btrfs superblock contains /dev/mapper/disk1-fmt paths. After
#    the real cryptsetup units reopen devices as /dev/mapper/disk1, btrfs
#    can't find them by path. `btrfs device scan` re-registers devices by
#    UUID, solving this. NixOS's scripted initrd does this automatically via
#    postDeviceCommands, but systemd-initrd has NO equivalent — you must add
#    the scan service yourself.
#
# 5. x-systemd.requires persists across switch-root. If btrfs-device-scan
#    only exists as an initrd service, systemd in stage 2 can't find it,
#    considers the requirement unmet, and UNMOUNTS /mnt/storage. The service
#    must exist in BOTH initrd (to gate the mount) and stage 2 (to satisfy
#    the dependency after switch-root).
{ lib, pkgs, ... }:
let
  passphrase = "testpassphrase";
  initrdSshFixtureDir = pkgs.path + "/nixos/tests/initrd-network-ssh";
  disks = [ "disk1" "disk2" "disk3" ];
in
{
  name = "remote-unlock";

  nodes.server = { config, pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];
    virtualisation.memorySize = 2048;

    environment.systemPackages = [ pkgs.btrfs-progs ];

    # Mount in initrd. The x-systemd options prevent the mount from starting
    # until btrfs-device-scan completes, which itself waits for all 3
    # cryptsetup units. This solves the "mount triggers on first device" race.
    virtualisation.fileSystems."/mnt/storage" = {
      device = "/dev/mapper/disk1";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    # Stage-2 copy of btrfs-device-scan. The x-systemd.requires on the mount
    # persists across switch-root, so the service must also exist in stage 2
    # or systemd will unmount /mnt/storage after switch-root.
    systemd.services.btrfs-device-scan = {
      description = "Scan for btrfs multi-device filesystems";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = [ pkgs.btrfs-progs ];
      script = "btrfs device scan";
    };

    boot.kernelParams = [
      "ip=${config.networking.primaryIPAddress}:::255.255.255.0::eth1:none"
    ];

    boot.initrd = {
      supportedFilesystems = [ "btrfs" ];

      systemd = {
        enable = true;
        network.enable = true;
        users.root.shell = "/bin/sh";
        extraBin = {
          cryptsetup = "${pkgs.cryptsetup}/bin/cryptsetup";
        };
        storePaths = [
          pkgs.cryptsetup
          pkgs.btrfs-progs
          pkgs.util-linux
        ];

        # One-shot initrd fixture: format empty drives as LUKS + btrfs RAID1
        # so the cryptsetup units have something to unlock.
        services.prepare-luks-btrfs-fixture = {
          description = "Prepare LUKS + btrfs RAID1 fixture for remote unlock test";
          requiredBy = map (d: "systemd-cryptsetup@${d}.service") disks;
          before = [ "cryptsetup-pre.target" ]
            ++ map (d: "systemd-cryptsetup@${d}.service") disks;
          after = [ "systemd-udevd.service" ];
          unitConfig.DefaultDependencies = false;
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
          path = [
            pkgs.coreutils
            pkgs.cryptsetup
            pkgs.btrfs-progs
            pkgs.util-linux
          ];
          script = ''
            set -eu

            # Wait for all drives and LUKS-format them
            for disk in ${lib.concatStringsSep " " disks}; do
              dev="/dev/disk/by-id/virtio-$disk"
              i=0
              while [ "$i" -lt 100 ]; do
                [ -b "$dev" ] && break
                sleep 0.1
                i=$((i + 1))
              done
              test -b "$dev"

              if ! cryptsetup isLuks "$dev" 2>/dev/null; then
                echo -n '${passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 "$dev"
              fi
            done

            # Open with -fmt suffix to avoid triggering systemd device/mount
            # units. Using the real names would make /dev/mapper/disk1 appear,
            # which could trigger dependent units prematurely.
            for disk in ${lib.concatStringsSep " " disks}; do
              echo -n '${passphrase}' | cryptsetup luksOpen --key-file=- "/dev/disk/by-id/virtio-$disk" "$disk-fmt"
            done

            # Create btrfs RAID1 across all drives
            if ! btrfs filesystem show /dev/mapper/disk1-fmt >/dev/null 2>&1; then
              mkfs.btrfs -f -d raid1 -m raid1 ${lib.concatMapStringsSep " " (d: "/dev/mapper/${d}-fmt") disks}
            fi

            # Close all — the real cryptsetup units will reopen them
            for disk in ${lib.concatStringsSep " " disks}; do
              cryptsetup luksClose "$disk-fmt"
            done
          '';
        };

        # After all LUKS devices are unlocked, scan for btrfs devices so
        # the kernel learns the filesystem members at their new paths
        # (/dev/mapper/disk* instead of the /dev/mapper/disk*-fmt paths
        # recorded at mkfs time). The mount unit depends on this via
        # x-systemd.requires/after options.
        services.btrfs-device-scan = {
          description = "Scan for btrfs multi-device filesystems";
          after = map (d: "systemd-cryptsetup@${d}.service") disks;
          requires = map (d: "systemd-cryptsetup@${d}.service") disks;
          before = [ "initrd-fs.target" ];
          wantedBy = [ "initrd-fs.target" ];
          unitConfig.DefaultDependencies = false;
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
          path = [ pkgs.btrfs-progs ];
          script = "btrfs device scan";
        };
      };

      # qemu-vm sets boot.initrd.luks.devices with mkVMOverride, so we must too.
      luks.devices = lib.mkVMOverride (
        lib.genAttrs disks (name: {
          device = "/dev/disk/by-id/virtio-${name}";
        })
      );

      network = {
        enable = true;
        ssh = {
          enable = true;
          port = 2222;
          authorizedKeys = [ (builtins.readFile (initrdSshFixtureDir + "/id_ed25519.pub")) ];
          hostKeys = [ (initrdSshFixtureDir + "/ssh_host_ed25519_key") ];
        };
      };
    };
  };

  nodes.client = { ... }: {
    environment.etc."ssh/test_id_ed25519" = {
      source = initrdSshFixtureDir + "/id_ed25519";
      mode = "0600";
    };
  };

  testScript = builtins.readFile ./remote-unlock.py;
}
