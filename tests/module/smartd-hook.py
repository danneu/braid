# Test: smartd-hook
#
# Intent: Verify the smartd exec hook script — its contents, invocation
#   behavior (creates flag file, starts alert service), and ack cleanup.
#
# Why it exists: The smartd-config test proves config composition (no duplicate
#   directives) and braid-smartd-alert tests the CLI flag-file model by touching
#   the file directly. Neither test invokes the actual hook script that smartd
#   would call. A broken script path, missing flag touch, or failed systemctl
#   start would go unnoticed.
#
# Scenario: Minimal NixOS machine with braid monitoring enabled. Locate the
#   smartd exec hook script from the rendered smartd.conf, verify its contents,
#   invoke it directly, and confirm braid ack cleans up.

import re

start_all()
machine.wait_for_unit("multi-user.target")

# --- Subtest 1: Hook script contents ---

with subtest("Smartd hook script has correct contents"):
    # Extract the --configfile= path from the smartd unit to find the
    # generated config, then extract the hook script path from it.
    exec_start = machine.succeed(
        "systemctl show smartd.service -p ExecStart --value"
    )
    match = re.search(r"--configfile=(\S+)", exec_start)
    assert match is not None, (
        f"No --configfile= in smartd ExecStart: {exec_start}"
    )
    config_path = match.group(1)
    config = machine.succeed(f"cat {config_path}")
    print(f"smartd.conf:\n{config}")

    hook_match = re.search(r"-M\s+exec\s+(\S+)", config)
    assert hook_match is not None, (
        f"No -M exec in smartd.conf: {config}"
    )
    hook_path = hook_match.group(1)

    script = machine.succeed(f"cat {hook_path}")
    assert "/var/lib/braid/smartd-alert" in script, (
        f"Hook must touch smartd-alert flag, got: {script}"
    )
    assert "braid-alert.service" in script, (
        f"Hook must start braid-alert.service, got: {script}"
    )

# --- Subtest 2: Hook invocation ---

with subtest("Smartd hook creates flag file and starts alert service"):
    machine.succeed("rm -f /var/lib/braid/smartd-alert")
    machine.succeed("rm -f /root/alert-fired")
    machine.execute("systemctl stop braid-alert.service 2>/dev/null || true")

    machine.succeed(f"{hook_path}")

    machine.succeed("test -f /var/lib/braid/smartd-alert")
    machine.succeed("systemctl is-active braid-alert.service")
    machine.succeed("test -f /root/alert-fired")

# --- Subtest 3: Ack clears smartd alert ---

with subtest("braid ack clears smartd alert and stops alert service"):
    machine.succeed("braid ack")
    machine.fail("test -f /var/lib/braid/smartd-alert")
    machine.fail("systemctl is-active braid-alert.service")

machine.shutdown()
