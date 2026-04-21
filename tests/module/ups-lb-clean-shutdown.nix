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
# Note: an inline test-only upsmon credential with `actions = [ "SET" ]`
# is provisioned here so upsrw can flip ups.status from the VM. The
# production upsmon user intentionally does not carry SET
# (reference/nut/docs/man/upsd.users.txt:78). A shared fixture for this
# pattern lives in plans/wip/forced-shutdown-recovery-proof.md --
# refactoring onto it is that plan's concern, not this plan's.
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

      # Seed pool.json -- initrd fixture bypasses `braid add` so there is
      # no membership file. `braid unlock` requires one.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1"}}}''
      ];

      # dummy-ups .dev fixture, on-utility-power at boot so the unlock
      # sequence completes cleanly; later flipped to OB+LB via upsrw.
      environment.etc."nut/ups.dev".text = ''
        device.mfr: Dummy
        device.model: UPS-v1-test
        ups.status: OL
        battery.charge: 100
        battery.charge.low: 10
      '';

      # Test-only upsmon user carrying actions = [ "SET" ] so upsrw can
      # drive ups.status from the test script. Separate from the
      # production upsmon credential provisioned by modules/braid/ups.nix:
      # per reference/nut/docs/man/upsd.users.txt:78 SET is only needed by
      # upsrw clients, and production upsmon does not need it.
      power.ups.users.testops = {
        passwordFile = toString (pkgs.writeText "testops.pass" "testpass");
        actions = [ "SET" ];
      };

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

      environment.systemPackages = [ pkgs.btrfs-progs pkgs.cryptsetup pkgs.nut ];
    };

  testScript = builtins.readFile ./ups-lb-clean-shutdown.py;
}
