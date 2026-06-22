# Test: ups-credential-lifecycle
#
# Intent: prove the upsmon token never reaches /nix/store, process argv,
# process env, journal output, or systemctl show, while runtime-rendered
# NUT configs remain owner-readable only.
#
# Why it exists: decision 020's "never enters the Nix store" claim is
# load-bearing. Refactors to the braid UPS wrapper or nixpkgs power.ups
# integration could silently regress from passwordFile paths to embedded
# secrets without changing user-visible behavior.
#
# Scenario: NAS boots, braid-ups-secrets.service mints
# /var/lib/braid/upsmon.pass, and power.ups renders /run/nut/* from that
# token. An operator inspecting nix-store closures, ps output, process
# environments, journal logs, or systemd unit metadata must not see it.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

machine.wait_for_unit("braid-ups-secrets.service", timeout=60)
machine.wait_for_unit("upsd.service", timeout=60)
machine.wait_for_unit("upsmon.service", timeout=60)
machine.wait_for_unit("upsdrv.service", timeout=60)


def show(unit, prop):
    return machine.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def assert_no_secret(label: str, haystack: str, token: str) -> None:
    assert token not in haystack, f"upsmon token leaked into {label}"


def assert_stat(path: str, expected: str) -> None:
    stat = machine.succeed(f"stat -c '%U:%G %a' {path}").strip()
    assert stat == expected, f"expected {path} to be {expected}, got {stat}"


with subtest("Secret file is 0600 root:root outside the Nix store"):
    assert_stat("/var/lib/braid/upsmon.pass", "root:root 600")
    token = machine.succeed("cat /var/lib/braid/upsmon.pass").strip()
    assert token != "", "upsmon token must not be empty"

with subtest("Secret generator carries the root sandbox"):
    unit = machine.succeed("systemctl cat braid-ups-secrets.service")
    assert "ProtectSystem=strict" in unit, (
        "ups secret unit must use ProtectSystem=strict:\n" + unit
    )
    assert "ReadWritePaths=/var/lib/braid" in unit, (
        "ups secret unit must keep braid state writable:\n" + unit
    )
    assert "CapabilityBoundingSet=" in unit, (
        "ups secret unit must drop all capabilities:\n" + unit
    )
    assert show("braid-ups-secrets.service", "PrivateNetwork") == "yes"
    assert show("braid-ups-secrets.service", "PrivateDevices") == "yes"
    assert show("braid-ups-secrets.service", "NoNewPrivileges") == "yes"

with subtest("Runtime NUT configs are restricted positive controls"):
    assert_stat("/run/nut/upsd.users", "root:root 400")
    assert_stat("/run/nut/upsmon.conf", "nutmon:root 400")

    users_conf = machine.succeed("cat /run/nut/upsd.users")
    upsmon_conf = machine.succeed("cat /run/nut/upsmon.conf")
    assert token in users_conf, "upsd.users must contain the rendered token"
    assert token in upsmon_conf, "upsmon.conf must contain the rendered token"

with subtest("Secret does not appear in the Nix store"):
    store_hits = machine.succeed(
        "TOKEN=$(cat /var/lib/braid/upsmon.pass); "
        "unit_paths=$(systemctl show "
        "-p FragmentPath "
        "-p DropInPaths "
        "-p ExecStart "
        "-p ExecStartPre "
        "upsd.service "
        "upsmon.service "
        "braid-ups-secrets.service "
        "| grep -Eo '/nix/store/[^ ;]+' "
        "| sort -u); "
        "closure_paths=$(nix-store -qR $unit_paths 2>/dev/null || true); "
        "printf '%s\\n' $unit_paths $closure_paths "
        "| sort -u "
        "| while IFS= read -r path; do "
        "if [ -e \"$path\" ]; then "
        "LC_ALL=C grep -RalF -- \"$TOKEN\" \"$path\" 2>/dev/null || true; "
        "fi; "
        "done"
    ).strip()
    assert store_hits == "", (
        "upsmon token appeared in Nix store paths:\n" + store_hits
    )

with subtest("Secret does not appear in process argv or env"):
    assert_no_secret("ps -eo args", machine.succeed("ps -eo args"), token)
    assert_no_secret("ps -eo cmd", machine.succeed("ps -eo cmd"), token)

    env_hits = machine.succeed(
        "TOKEN=$(cat /var/lib/braid/upsmon.pass); "
        "grep -alF -- \"$TOKEN\" /proc/[0-9]*/environ 2>/dev/null || true"
    ).strip()
    assert env_hits == "", (
        "upsmon token appeared in process environment files:\n" + env_hits
    )

with subtest("Secret does not appear in journal or systemd metadata"):
    journal = machine.succeed(
        "journalctl -b 0 "
        "-u upsd.service "
        "-u upsmon.service "
        "-u braid-ups-secrets.service "
        "--no-pager"
    )
    assert_no_secret("NUT/braid UPS journal", journal, token)

    systemctl_show = machine.succeed(
        "systemctl show "
        "upsd.service "
        "upsmon.service "
        "braid-ups-secrets.service"
    )
    assert_no_secret("systemctl show", systemctl_show, token)

machine.shutdown()
