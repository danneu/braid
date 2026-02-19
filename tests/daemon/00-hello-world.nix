# Test: daemon-hello-world
#
# What: Starts the btrnasd daemon and sends a ping over its Unix socket.
#
# Why: Proves the end-to-end chain works: Go binary builds, systemd service
# starts, socket accepts connections, NDJSON request/response round-trips.
# No disks, no LUKS, no btrfs — just the daemon.
#
# Dependencies: VM infra (hello-world).
{
  name = "daemon-hello-world";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/btrnas ];
    btrnas.daemon.enable = true;
    environment.systemPackages = [ pkgs.socat ];
  };

  testScript = builtins.readFile ./00-hello-world.py;
}
