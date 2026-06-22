# Test: braid-lock-systemd-stop
#
# What: braid-online ExecStop waits for the pool lock up to the configured
# deadline and reports deadline expiry distinctly.
#
# Why: Shutdown must not SIGKILL braid mid-lock, and lock contention should be
# visible in the journal.
{ braid }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-lock-systemd-stop";

  nodes.machine =
    { ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for systemd-stop lock test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        lockSystemdStopDeadlineSecs = 5;
      };

      systemd.tmpfiles.rules = [
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

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
      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./braid-lock-systemd-stop.py;
}
