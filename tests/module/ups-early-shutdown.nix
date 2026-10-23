# Test: ups-early-shutdown
#
# What: Boots the VM with braid.ups on the shared dummy-ups fixture and a
# short `shutdownAfter`, running the production braid-ups-guard.service
# unchanged (its user, sandbox, and preserved runtime directory). A short
# outage that ends before the timer leaves the host up. A long outage, with
# the guard restarted partway through, forces a shutdown through upsmon on
# the original deadline. After the reboot the previous boot's journal shows
# the guard's trigger line, braid-online's ExecStop, and ups-killpower.
#
# Why: the timer is the only early-shutdown path braid ships
# (plans/impl/2026-10-22-1122-ups-on-battery-shutdown-policy.md). Unit tests
# cover the decision and the loop; only a VM proves that the unprivileged
# guard can reach upsd, signal upsmon through `upsmon -c fsd`, and that the
# resulting shutdown is the same clean, killpower-armed path as LB.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
in
{
  name = "ups-early-shutdown";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase;
          diskNames = [ "disk1" ];
          description = "Prepare LUKS + btrfs fixture for UPS early-shutdown test";
        })
        (import ./lib/ups-fixture.nix { })
      ];

      braid = {
        enable = true;
        package = braid;
        # Long enough that a guard restart partway through lands well clear of
        # both the original deadline and one timer length after the restart.
        # The fixture reports 100 % charge, so the default floor never fires.
        ups.shutdownAfter = 90;
      };

      systemd.tmpfiles.rules = [
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"}}}''
      ];

      # Read the previous boot's journal after the forced shutdown.
      services.journald.extraConfig = "Storage=persistent";

      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];
      virtualisation.memorySize = 2048;
    };

  testScript = builtins.readFile ./ups-early-shutdown.py;
}
