# Test: pool-bound-services
#
# What: Verifies that braid.poolBoundServices stamps the documented
# long-running consumer contract onto a listed service without replacing the
# service's existing boot edge.
#
# Why: SMB/NFS users previously had to hand-write WantedBy, BindsTo, After, and
# ConditionPathIsMountPoint on every pool consumer. Missing any field can leave
# the service stopped after unlock, unordered before lock, or serving the
# offline mountpoint.
#
# Scenario: A dummy service has only its normal multi-user.target boot edge and
# an ExecStart that holds /mnt/storage busy. The option should condition-skip it
# while locked, start it on unlock, put it in BoundBy for lock teardown, and
# restart it on a later unlock.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "pool-bound-services";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for pool-bound-services test";
          supportedFilesystems = [ "btrfs" ];
          preCloseScript = ''
            mkdir -p /tmp/fixture-mount
            mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
            touch /tmp/fixture-mount/.consumer-lock
            chmod 0644 /tmp/fixture-mount/.consumer-lock
            sync
            umount /tmp/fixture-mount
          '';
        })
      ];

      braid = {
        enable = true;
        package = braid;
        poolBoundServices = [ "dummy-pool-consumer" ];
      };

      systemd.tmpfiles.rules = [
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      systemd.services.dummy-pool-consumer = {
        description = "Fake long-running consumer that holds /mnt/storage busy";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = pkgs.writeShellScript "dummy-pool-consumer" ''
            set -e
            exec 3</mnt/storage/.consumer-lock
            sleep 300
          '';
        };
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript = builtins.readFile ./pool-bound-services.py;
}
