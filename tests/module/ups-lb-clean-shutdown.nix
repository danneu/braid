# Test: ups-lb-clean-shutdown
#
# What: Boots the VM with braid.ups.enable = true and a dummy-ups driver
# in dummy-once mode (.dev file). Unlocks the pool, writes a canary file,
# then flips ups.status to "OB LB" via upsrw. Upsmon sees critical,
# invokes SHUTDOWNCMD, systemd unwinds braid-online.service, btrfs
# unmounts cleanly, LUKS closes cleanly. The VM reboots; we verify the
# canary survived and the previous boot's journal shows ExecStop ran.
#
# Why: v1 guarantee (1) is "orderly shutdown before battery exhaustion
# in ordinary mounted operation." Without this test, the safety-core
# shipping claim is hollow -- every other piece of wiring (SHUTDOWNCMD
# override, braid-online's ExecStop hook from decision 018, systemd's
# shutdown sequence) could be right in isolation while still failing
# under the real upsmon critical trigger.
#
# Imports the shared `lib/ups-fixture.nix` so the dummy-ups driver mode,
# .dev contents, and the test-only `testops` SET credential stay
# consistent across this Plan 1 test and the Plan 3 forced-shutdown
# matrix (`ups-lb-during-{replace,remove,remove-missing,balanced-add}`).
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
in
{
  name = "ups-lb-clean-shutdown";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase;
          diskNames = [ "disk1" ];
          description = "Prepare LUKS + btrfs fixture for UPS LB shutdown test";
        })
        (import ./lib/ups-fixture.nix { })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      # Seed pool.json -- initrd fixture bypasses `braid add` so there is
      # no membership file. `braid unlock` requires one.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1"}}}''
      ];

      # Persist journal across reboots so the post-reboot subtest can
      # read the previous boot's ExecStop log via `journalctl -b -1`.
      services.journald.extraConfig = "Storage=persistent";

      # Override braid-unlock.service script so VM tests can unlock
      # without systemd-ask-password.
      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      virtualisation.emptyDiskImages = [
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      ];
      virtualisation.memorySize = 2048;
    };

  testScript = builtins.readFile ./ups-lb-clean-shutdown.py;
}
