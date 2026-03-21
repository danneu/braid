# Test: smartd config composition
#
# Intent: Verify that braid's smartd notification config survives the presence
#   of a sendmail wrapper without producing duplicate directives.
#
# Why it exists: The NixOS smartd module auto-enables mail notifications when a
#   sendmail wrapper exists, prepending its own -m/-M exec to every config line.
#   braid must explicitly suppress this to keep a clean config.
#
# Scenario: Machine boots with braid monitoring + sendmail wrapper. Read the
#   generated smartd.conf, assert each config line has exactly one -m <nomailer>
#   and one -M exec pointing to braid's script.

import re

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("smartd service is enabled"):
    machine.succeed("systemctl is-enabled smartd.service")

with subtest("smartd config has exactly one notification handler per line"):
    # Extract the --configfile= path from the smartd unit
    exec_start = machine.succeed(
        "systemctl show smartd.service -p ExecStart --value"
    )
    match = re.search(r"--configfile=(\S+)", exec_start)
    assert match is not None, f"No --configfile= in ExecStart: {exec_start}"
    config_path = match.group(1)
    config = machine.succeed(f"cat {config_path}")
    print(f"smartd.conf:\n{config}")

    # Check each directive line (DEFAULT and DEVICESCAN)
    for line in config.strip().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue

        # Each config line must have exactly one -m directive
        m_count = len(re.findall(r"\s-m\s", f" {line} "))
        assert m_count == 1, (
            f"Expected exactly 1 '-m' directive, found {m_count} in: {line}"
        )

        # The -m directive must be <nomailer>, not an address
        assert "-m <nomailer>" in line, (
            f"Expected '-m <nomailer>', not found in: {line}"
        )

        # Each config line must have exactly one -M exec
        exec_count = len(re.findall(r"-M\s+exec\s", line))
        assert exec_count == 1, (
            f"Expected exactly 1 '-M exec', found {exec_count} in: {line}"
        )

        # The -M exec must point to braid's script, not the NixOS smartd-notify
        assert "braid-smartd-alert" in line, (
            f"Expected braid-smartd-alert in -M exec, not found in: {line}"
        )
        assert "smartd-notify" not in line, (
            f"NixOS smartd-notify.sh should not appear in: {line}"
        )

machine.shutdown()
