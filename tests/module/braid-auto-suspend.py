# Test: braid-sleep module configuration
#
# Intent: Verify that braid.autoSuspend produces the correct autosuspend config
#   with BraidPool, SSH, Smb checks and BtrfsScrub wakeup.
#
# Why it exists: The autosuspend integration is the wiring between braid's
#   idle check and the system suspend daemon. If the check command is wrong
#   or uses unqualified paths, the NAS will either never sleep or sleep
#   during operations.
#
# Scenario: NixOS machine with braid.autoSuspend.enable = true and samba enabled.
#   Read the generated autosuspend config file and verify all expected
#   sections and values are present.

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("autosuspend service is active"):
    machine.succeed("systemctl is-active autosuspend.service")

with subtest("Read autosuspend config"):
    # The autosuspend NixOS module generates a config file in the nix store.
    # Find it via the service's ExecStart line.
    exec_start = machine.succeed(
        "systemctl show autosuspend.service -p ExecStart --value"
    ).strip()
    # ExecStart contains: /nix/store/.../bin/autosuspend -l ... -c /nix/store/.../autosuspend.conf daemon
    # Extract the config path after -c
    parts = exec_start.split()
    config_path = None
    for i, part in enumerate(parts):
        if part == "-c" and i + 1 < len(parts):
            config_path = parts[i + 1]
            break
    assert config_path is not None, "Could not find -c flag in ExecStart: " + exec_start
    config = machine.succeed("cat " + config_path)
    print("autosuspend config:\n" + config)

with subtest("BraidPool check exists with braid idle command"):
    assert "[check.BraidPool]" in config, "Missing [check.BraidPool] in config"
    assert "braid idle" in config, "Missing 'braid idle' in config"

with subtest("BraidPool command uses fully qualified store paths"):
    # Extract the command line from the config
    in_braid_section = False
    command_line = None
    for line in config.splitlines():
        if line.strip() == "[check.BraidPool]":
            in_braid_section = True
        elif in_braid_section and line.strip().startswith("["):
            break
        elif in_braid_section and line.strip().startswith("command"):
            command_line = line
            break
    assert command_line is not None, "Could not find command in [check.BraidPool]"
    assert "/nix/store/" in command_line, (
        "BraidPool command must use fully qualified /nix/store/ paths, got: " + command_line
    )
    # Specifically: timeout and bash must be store paths
    assert "bin/timeout" in command_line, "Missing timeout in command: " + command_line
    assert "bin/bash" in command_line, "Missing bash in command: " + command_line

with subtest("SSH check exists (always on)"):
    assert "[check.SSH]" in config, "Missing [check.SSH] in config"
    assert "ports=22" in config, "Missing 'ports=22' in config"

with subtest("Smb check exists (auto-detected from samba)"):
    assert "[check.Smb]" in config, "Missing [check.Smb] in config"

with subtest("BtrfsScrub wakeup exists"):
    assert "[wakeup.BtrfsScrub]" in config, "Missing [wakeup.BtrfsScrub] in config"
    assert "btrfs-scrub@" in config, "Missing btrfs-scrub@ match pattern in config"

with subtest("General settings"):
    assert "idle_time=900" in config, "Missing idle_time=900 in config"
    assert "interval=60" in config, "Missing interval=60 in config"

with subtest("WoL link file exists for configured interface"):
    # The NixOS networking module creates a systemd .link file that sets
    # WakeOnLan=magic. Verify the link file exists and contains the right
    # directive. (ethtool can't be tested in QEMU — virtual NICs don't
    # support real WoL.)
    link_content = machine.succeed("cat /etc/systemd/network/40-eth0.link")
    assert "WakeOnLan" in link_content, "Missing WakeOnLan in link file: " + link_content

machine.shutdown()
