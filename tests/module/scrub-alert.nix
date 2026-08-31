# Test: scrub-alert
#
# What: Verifies that a genuinely failed maintenance scrub raises braid's
# user-facing alert via onFailure (flag -> beep -> status -> latch -> ack),
# while a deliberate lock-time cancel and the clean / corruption exit codes
# stay silent.
#
# Why: braid-scrub.service had no failure alerting -- a scrub that failed to
# run or complete left the unit `failed` with no operator signal. onFailure now
# wires it to braid-alert, but btrfs exits 1 for BOTH a real cancel and a
# genuine failure, exits 3 for scrub-found corruption (which alerts via the
# device-stats poll, not here), and braid exits 4 when the busy gate skipped the
# run. Only a behavioral test proves the cancel marker and SuccessExitStatus
# keep onFailure scoped to real execution failure.
#
# Scenario: Two nodes, each with a 2-disk RAID1 pool and monitor enabled.
#   fail:   exit-code-parameterized scrub (mkForce ExecStart reads the code from
#           a file). Exit 1 raises and clears the alert end-to-end; exit 3,
#           exit 0 and exit 4 (busy skip) stay silent.
#   cancel: dm-delay-backed REAL scrub, cancelled mid-run by `braid lock`. The
#           real btrfs-exit-1-on-cancel + cancel-request marker path must resolve
#           to Result=success and raise no alert (the fake `sleep 300` scrub used
#           elsewhere is SIGTERM-clean and would not exercise this path).
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];

  poolJson = ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}'';

  unlockScript = lib.mkForce ''
    printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
  '';

  monitorBraid = {
    enable = true;
    package = braid;
    autoScrub.enable = true;
    monitor.enable = true;
    monitor.alertCommand = "touch /root/alert-fired";
  };
in
{
  name = "scrub-alert";

  # --- fail node: exit-code-parameterized scrub ---
  nodes.fail =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for scrub-alert tests";
        })
      ];

      braid = monitorBraid;

      systemd.tmpfiles.rules = [
        poolJson
      ];

      systemd.services.braid-unlock.script = unlockScript;

      # Deterministic, exit-code-parameterized scrub: each subtest writes the
      # desired btrfs main-process exit code to /run/braid-test-scrub-exit and
      # restarts the service. Absent file defaults to 1 (failure). mkForce
      # replaces the real `braid scrub-resume-or-start` ExecStart so no real
      # btrfs runs on this node.
      systemd.services.braid-scrub.serviceConfig.ExecStart = lib.mkForce (
        toString (
          pkgs.writeShellScript "param-scrub" ''
            exit "$(cat /run/braid-test-scrub-exit 2>/dev/null || echo 1)"
          ''
        )
      );
      # No-op ExecStop so each run's Result reflects only the chosen main-process
      # exit -- no cancel-marker / scrub-cancel side effects on this node.
      systemd.services.braid-scrub.serviceConfig.ExecStop = lib.mkForce "${pkgs.coreutils}/bin/true";

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

  # --- cancel node: dm-delay-backed real scrub, cancelled by lock ---
  nodes.cancel =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
      ];

      braid = monitorBraid;

      systemd.services.braid-unlock.script = unlockScript;

      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.lvm2
      ];
    };

  testScript = builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./scrub-alert.py;
}
