# Test: daemon-hello-world
#
# What: Starts the braid daemon via systemd socket activation and sends
# requests over its Unix socket.
#
# Why: Proves the end-to-end chain works: Rust binary builds, systemd socket
# activation triggers the daemon on first connection, NDJSON request/response
# round-trips. No disks, no LUKS, no btrfs — just the daemon.
#
# Dependencies: VM infra (hello-world).
{ braid }:
{
  name = "daemon-hello-world";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];
    braid.daemon.enable = true;
    braid.daemon.package = braid;
    environment.systemPackages = [ pkgs.socat ];
  };

  testScript = builtins.readFile ./00-hello-world.py;
}
