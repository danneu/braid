# Test: braid-doctor-ups -- UPS-adjacent configuration checks
#
# What: Boots a VM with `braid.ups.enable = true` and a dummy-ups
# driver, mounts a pool via `braid unlock`, then disables
# `braid-online.service` while the pool is still mounted. Asserts
# `braid doctor` surfaces the braid_online_active check as Fail
# (high severity) with the expected "UPS shutdown will not unmount
# the pool" message.
#
# Why: this is the critical UPS-adjacent fault -- without
# `braid-online.service` active, `SHUTDOWNCMD = systemctl poweroff`
# does not run the ExecStop that calls `braid lock`, and Plan 1's
# safety guarantee silently breaks. The doctor check is the
# operator's only asynchronous warning (no alert-model integration
# in v1 per ADR 020). A regression that downgraded this to Warn, or
# turned it into Skip on a mounted pool, would remove the last
# safety net.
#
# Covers the check end-to-end under a real NUT stack rather than
# only the mocked unit tests.
{ braid }:
{
  name = "braid-doctor-ups";

  nodes.machine =
    { pkgs, lib, ... }:
    let
      passphrase = "testpassphrase";
    in
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase;
          diskNames = [ "disk1" ];
          description = "Prepare LUKS + btrfs fixture for doctor UPS test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        ups = {
          enable = true;
          name = "ups";
          driver = "dummy-ups";
          port = "ups.dev";
        };
      };

      # Dummy-ups input file -- single OL state, no upsrw flips needed.
      environment.etc."nut/ups.dev".text = ''
        ups.status: OL
        battery.charge: 100
        battery.runtime: 1800
        ups.load: 10
      '';

      # Seed pool.json -- initrd fixture bypasses `braid add`.
      systemd.tmpfiles.rules = [
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"}}}''
      ];

      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.util-linux
      ];

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 2048;
    };

  testScript = builtins.readFile ./braid-doctor-ups.py;
}
