/*
  Intent: braid lock (user-initiated) and `systemctl stop braid-online.service`
    (shutdown / manual ExecStop) both stop bound pool consumers and unmount
    cleanly when a long-running consumer holds /mnt/storage busy.
  Why it exists: regression guard for the EBUSY-on-busy-mount class of bug
    (samba on caja, future nfs/syncthing). The user-initiated lock path
    relies on cmd_lock iterating BoundBy braid-online.service through
    OnlineStateOps::list_bound_by; the ExecStop path relies on systemd's
    BindsTo cascade stopping consumers before `braid lock --systemd-stop`
    runs cmd_lock.
  Scenario: pool unlocked with a fake consumer service holding fd 3 on
    /mnt/storage/.consumer-lock (BindsTo+After+wantedBy braid-online.service,
    ConditionPathIsMountPoint=/mnt/storage). Cycle 1 runs `braid lock` and
    asserts the consumer is stopped, the mount is gone, and LUKS mappers
    are closed. Cycle 2 unlocks again, runs `systemctl stop
    braid-online.service`, and asserts the same teardown via the ExecStop
    reentry path.
*/
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
  name = "lock-stops-bound-consumers";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for bound-consumer lock test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      # Fake long-running consumer that holds /mnt/storage busy. Conforms to
      # the contract documented in docs/decisions/018-systemd-lifecycle.md
      # for "long-running services holding open files" -- BindsTo + After +
      # wantedBy braid-online.service plus ConditionPathIsMountPoint as
      # defense-in-depth against direct activation when the pool is offline.
      systemd.services.dummy-pool-consumer = {
        description = "Fake long-running consumer that holds /mnt/storage busy";
        wantedBy = [ "braid-online.service" ];
        after = [ "braid-online.service" ];
        bindsTo = [ "braid-online.service" ];
        unitConfig.ConditionPathIsMountPoint = "/mnt/storage";
        serviceConfig = {
          Type = "simple";
          ExecStart = pkgs.writeShellScript "dummy-pool-consumer" ''
            exec 3>/mnt/storage/.consumer-lock
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

  testScript = builtins.readFile ./lock-stops-bound-consumers.py;
}
