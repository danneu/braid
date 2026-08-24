# Test: braid shell completion
#
# What: Validates that braid generates shell completion registration scripts
# for bash/zsh/fish, and that bash completions produce correct candidates for
# subcommands, flags, and role-appropriate member names from pool membership.
#
# Why: Shell completions are a core UX feature. Existing-member arguments must
# offer known names, while new-disk spec arguments must not offer bare names
# that their parsers reject.
#
# Dependencies: Rust braid binary with clap_complete CompleteEnv support.
{ braid }:
{
  name = "shell-completion";

  nodes.machine =
    { pkgs, ... }:
    {
      environment.systemPackages = [
        braid
        pkgs.bash
        pkgs.fish
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./shell-completion.py;
}
