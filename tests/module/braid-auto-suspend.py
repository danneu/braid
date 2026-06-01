# Test: braid-sleep module configuration
#
# Intent: Verify that braid.autoSuspend produces the correct autosuspend config
#   with BraidPool, BraidWol, SSH, Smb checks and BtrfsScrub wakeup.
#
# Why it exists: The autosuspend integration is the wiring between braid's
#   idle/WoL checks and the system suspend daemon. If a check command is wrong
#   or uses unqualified paths, the NAS will either never sleep, sleep during
#   operations, or sleep without a verified wake path.
#
# Scenario: NixOS machine with braid.autoSuspend.enable = true and samba enabled.
#   Read the generated autosuspend config file and verify all expected
#   sections and values are present.

import json
import re
import time

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

def command_line_for(section):
    in_section = False
    for line in config.splitlines():
        if line.strip() == f"[check.{section}]":
            in_section = True
        elif in_section and line.strip().startswith("["):
            break
        elif in_section and line.strip().startswith("command"):
            return line
    return None

with subtest("BraidPool check exists with braid idle command"):
    assert "[check.BraidPool]" in config, "Missing [check.BraidPool] in config"
    assert "braid idle" in config, "Missing 'braid idle' in config"

with subtest("BraidPool command uses fully qualified store paths"):
    # Extract the command line from the config
    braid_pool_command_line = command_line_for("BraidPool")
    assert braid_pool_command_line is not None, "Could not find command in [check.BraidPool]"
    assert "/nix/store/" in braid_pool_command_line, (
        "BraidPool command must use fully qualified /nix/store/ paths, got: " + braid_pool_command_line
    )
    # Specifically: timeout and bash must be store paths
    assert "bin/timeout" in braid_pool_command_line, "Missing timeout in command: " + braid_pool_command_line
    assert "bin/timeout -k 2 10" in braid_pool_command_line, (
        "BraidPool command must escalate TERM to KILL after the 10s timeout, got: "
        + braid_pool_command_line
    )
    assert "bin/bash" in braid_pool_command_line, "Missing bash in command: " + braid_pool_command_line

with subtest("BraidWol check exists with braid wol-ready command"):
    # Intent: Ensure autosuspend config includes the WoL gate beside BraidPool.
    # Why it exists: auto-suspend must re-check live WoL every suspend cycle,
    #   not only when an operator manually runs doctor.
    # Scenario: NixOS renders services.autosuspend checks for a host with
    #   braid.autoSuspend.enable = true.
    assert "[check.BraidWol]" in config, "Missing [check.BraidWol] in config"
    assert "braid wol-ready" in config, "Missing 'braid wol-ready' in config"

with subtest("BraidWol command uses fully qualified store paths"):
    # Intent: Pin the BraidWol ExternalCommand store-path and timeout shape.
    # Why it exists: autosuspend runs outside braid's wrapper PATH; a missing
    #   store path or outer timeout can fail open.
    # Scenario: autosuspend invokes the generated BraidWol check in its daemon
    #   environment.
    braid_wol_command_line = command_line_for("BraidWol")
    assert braid_wol_command_line is not None, "Could not find command in [check.BraidWol]"
    assert "/nix/store/" in braid_wol_command_line, (
        "BraidWol command must use fully qualified /nix/store/ paths, got: " + braid_wol_command_line
    )
    assert "bin/timeout" in braid_wol_command_line, "Missing timeout in command: " + braid_wol_command_line
    assert "bin/timeout -k 2 10" in braid_wol_command_line, (
        "BraidWol command must escalate TERM to KILL after the 10s timeout, got: "
        + braid_wol_command_line
    )
    assert "bin/bash" in braid_wol_command_line, "Missing bash in command: " + braid_wol_command_line

with subtest("SSH check exists (always on)"):
    assert "[check.SSH]" in config, "Missing [check.SSH] in config"
    assert "ports=22" in config, "Missing 'ports=22' in config"

with subtest("Smb check exists (auto-detected from samba)"):
    assert "[check.Smb]" in config, "Missing [check.Smb] in config"

with subtest("BtrfsScrub wakeup exists"):
    assert "[wakeup.BtrfsScrub]" in config, "Missing [wakeup.BtrfsScrub] in config"
    assert "braid-scrub" in config, "Missing braid-scrub match pattern in config"

with subtest("General settings"):
    assert "idle_time=900" in config, "Missing idle_time=900 in config"
    assert "interval=60" in config, "Missing interval=60 in config"

with subtest("braid config records WoL interface"):
    braid_config = json.loads(machine.succeed("cat /etc/braid/config.json"))
    assert braid_config["auto_suspend"]["wol_interface"] == "eth0", braid_config

with subtest("BraidPool command fail-closes when braid idle overruns the inner timeout"):
    # Intent: Pin that a signal-killable overrun of `braid idle` past the
    #   inner `timeout -k 2 10` produces autosuspend exit 0 (block suspend),
    #   not a non-zero timeout status.
    # Why it exists: The check used to wrap `bash -c '! braid idle'` with an
    #   outer `timeout`. On overrun the outer `timeout` killed bash, exited
    #   124, and the `!` never inverted -- autosuspend treated it as no
    #   activity and allowed suspend. That broke the fail-closed invariant
    #   in docs/design/decisions/016-auto-suspend.md.
    # Scenario: substitute a hanging stub for `braid idle` in the configured
    #   command, run it, and verify the inner `timeout` fires and `!` inverts
    #   to 0 before the outer watchdog fires.
    assert braid_pool_command_line is not None, "BraidPool command not extracted"
    command_value = braid_pool_command_line.split("=", 1)[1].strip()

    machine.succeed(
        "printf '%s\\n%s\\n%s\\n' '#!/bin/sh' 'trap \"\" TERM' 'exec sleep 60' "
        "> /tmp/braid-hang-stub "
        "&& chmod +x /tmp/braid-hang-stub"
    )

    pattern = r"/nix/store/[^ ]+/bin/braid idle"
    modified, n = re.subn(pattern, "/tmp/braid-hang-stub", command_value)
    assert n == 1, (
        "Expected exactly one /nix/store/.../bin/braid idle match in BraidPool "
        f"command, got {n}. command_value={command_value!r}"
    )

    start = time.monotonic()
    rc, out = machine.execute("timeout -k 2 18 " + modified)
    elapsed = time.monotonic() - start

    assert rc == 0, (
        f"Expected exit 0 (! inverts inner timeout's non-zero result) but got {rc}. "
        f"output={out!r}"
    )
    assert elapsed < 15, (
        f"Expected wall time <15s (inner `timeout -k 2 10` should fire), got "
        f"{elapsed:.1f}s. The outer watchdog likely tripped, meaning the "
        f"inner timeout did not bound the stub."
    )

with subtest("BraidWol command fail-closes when braid wol-ready overruns the inner timeout"):
    # Intent: Pin that a signal-killable overrun of `braid wol-ready` past the
    #   inner `timeout -k 2 10` produces autosuspend exit 0 (block suspend).
    # Why it exists: the WoL gate is safety-critical; an unbounded or
    #   incorrectly-inverted check could let the NAS sleep without proving it
    #   can wake.
    # Scenario: substitute a hanging stub for `braid wol-ready` in the
    #   configured command and verify `!` inverts the timeout result.
    assert braid_wol_command_line is not None, "BraidWol command not extracted"
    command_value = braid_wol_command_line.split("=", 1)[1].strip()

    machine.succeed(
        "printf '%s\\n%s\\n%s\\n' '#!/bin/sh' 'trap \"\" TERM' 'exec sleep 60' "
        "> /tmp/braid-wol-hang-stub "
        "&& chmod +x /tmp/braid-wol-hang-stub"
    )

    pattern = r"/nix/store/[^ ]+/bin/braid wol-ready"
    modified, n = re.subn(pattern, "/tmp/braid-wol-hang-stub", command_value)
    assert n == 1, (
        "Expected exactly one /nix/store/.../bin/braid wol-ready match in BraidWol "
        f"command, got {n}. command_value={command_value!r}"
    )

    start = time.monotonic()
    rc, out = machine.execute("timeout -k 2 18 " + modified)
    elapsed = time.monotonic() - start

    assert rc == 0, (
        f"Expected exit 0 (! inverts inner timeout's non-zero result) but got {rc}. "
        f"output={out!r}"
    )
    assert elapsed < 15, (
        f"Expected wall time <15s (inner `timeout -k 2 10` should fire), got "
        f"{elapsed:.1f}s. The outer watchdog likely tripped, meaning the "
        f"inner timeout did not bound the stub."
    )

with subtest("WoL link file exists for configured interface"):
    # The NixOS networking module creates a systemd .link file that sets
    # WakeOnLan=magic. Verify the link file exists and contains the right
    # directive. (ethtool can't be tested in QEMU — virtual NICs don't
    # support real WoL.)
    link_content = machine.succeed("cat /etc/systemd/network/40-eth0.link")
    assert "WakeOnLan" in link_content, "Missing WakeOnLan in link file: " + link_content

def doctor_wol_check(mode):
    machine.succeed(f"printf '{mode}\\n' > /tmp/braid-wol-mode")
    result = machine.execute("braid doctor --json")
    report = json.loads(result[1])
    checks = {c["name"]: c for c in report["checks"]}
    return result[0], checks["wake_on_lan"], result[1]

with subtest("doctor wake_on_lan uses overridden ethtool package"):
    exit_code, check, raw = doctor_wol_check("g")
    assert exit_code == 0, f"doctor should pass with Wake-on: g: {raw}"
    assert check["status"] == "ok", check
    assert "Wake-on: g" in check["message"], check

with subtest("doctor wake_on_lan fails when overridden ethtool reports disabled"):
    exit_code, check, raw = doctor_wol_check("d")
    assert exit_code != 0, f"doctor should fail with Wake-on: d: {raw}"
    assert check["status"] == "fail", check
    assert "Wake-on: d" in check["message"], check

with subtest("braid wol-ready uses overridden ethtool package"):
    # Intent: Verify the hidden autosuspend gate succeeds through the same
    #   wrapper-provided fake ethtool package as doctor.
    # Why it exists: unit tests cannot prove Nix wrapper PATH wiring or config
    #   JSON plumbing for the hidden command.
    # Scenario: autosuspend invokes `braid wol-ready` when ethtool reports
    #   `Wake-on: g`.
    machine.succeed("printf 'g\\n' > /tmp/braid-wol-mode")
    rc, out = machine.execute("braid wol-ready")
    assert rc == 0, f"wol-ready should pass with Wake-on: g, got {rc}: {out}"

with subtest("braid wol-ready fails when overridden ethtool reports disabled"):
    # Intent: Verify the hidden autosuspend gate exits non-zero when live WoL
    #   state is unsafe.
    # Why it exists: autosuspend depends on this exit code to block sleep via
    #   the ExternalCommand inversion.
    # Scenario: ethtool reports `Wake-on: d` immediately before an idle
    #   autosuspend decision.
    machine.succeed("printf 'd\\n' > /tmp/braid-wol-mode")
    rc, out = machine.execute("braid wol-ready")
    assert rc == 1, f"wol-ready should fail with Wake-on: d, got {rc}: {out}"

machine.shutdown()
