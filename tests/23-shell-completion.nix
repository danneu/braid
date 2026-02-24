# Test: braid shell completion
#
# What: Validates that braid generates shell completion registration scripts
# for bash/zsh/fish, and that bash completions produce correct candidates for
# subcommands, flags, and dynamic disk paths from config.
#
# Why: Shell completions are a core UX feature. They must return the correct
# subcommands, flags, and config-driven disk paths. The --config override must
# be respected during completion so non-default configs get correct candidates.
#
# Dependencies: Rust braid binary with clap_complete CompleteEnv support.
{ braid }:
{
  name = "shell-completion";

  nodes.machine = { pkgs, ... }: {
    environment.systemPackages = [
      braid
      pkgs.bash
      pkgs.fish
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./shell-completion.py;
}
