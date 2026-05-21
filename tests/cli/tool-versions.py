import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

# Load expected versions from Nix evaluation
expected = json.loads(machine.succeed("cat /etc/braid/expected-versions.json"))

# Provenance: assert tools resolve to /nix/store/ paths (not ambient PATH leaks)
for tool in ["btrfs", "cryptsetup", "findmnt", "lsblk", "mountpoint", "upsc"]:
    with subtest(f"{tool} provenance"):
        path = machine.succeed(f"readlink -f $(command -v {tool})").strip()
        assert path.startswith("/nix/store/"), f"{tool} not from nix store: {path}"

# Exact version assertions — drift = parser contract violation
with subtest("btrfs-progs version"):
    version = machine.succeed("btrfs --version").strip().splitlines()[0]
    exp = f"btrfs-progs v{expected['btrfsProgs']}"
    assert version == exp, f"expected {exp!r}, got {version!r}"

with subtest("cryptsetup version"):
    version = machine.succeed("cryptsetup --version").strip()
    exp = f"cryptsetup {expected['cryptsetup']}"
    assert version.startswith(exp), f"expected prefix {exp!r}, got {version!r}"

with subtest("util-linux version"):
    version = machine.succeed("findmnt --version").strip()
    exp = f"findmnt from util-linux {expected['utilLinux']}"
    assert version == exp, f"expected {exp!r}, got {version!r}"

with subtest("nut upsc version"):
    version = machine.succeed("upsc -V").strip()
    exp = f"Network UPS Tools upsc {expected['nut']}"
    assert version.startswith(exp), f"expected prefix {exp!r}, got {version!r}"

# Rust binary wrapper provenance
with subtest("braid provenance"):
    module_braid = machine.succeed("readlink -f $(command -v braid)").strip()
    assert module_braid.startswith("/nix/store/"), (
        f"braid not from nix store: {module_braid}"
    )

ups_config = json.dumps(
    {
        "mount_point": "/mnt/storage",
        "pool_access_group": "storage",
        "systemd_lifecycle": True,
        "ups": {"name": "ups"},
    }
)
machine.succeed(
    "printf '%s\\n' "
    + shlex.quote(ups_config)
    + " > /tmp/tool-versions-ups.json"
)


def assert_wrapper_finds_upsc(label, braid_command):
    stdout_path = f"/tmp/{label}.out"
    stderr_path = f"/tmp/{label}.err"
    exit_code = machine.execute(
        f"PATH=/nonexistent {braid_command} --config /tmp/tool-versions-ups.json "
        f"ups status --json >{stdout_path} 2>{stderr_path}"
    )[0]
    assert exit_code != 0, f"{label}: expected non-zero query failure; got 0"
    parsed = json.loads(machine.succeed(f"cat {stdout_path}"))
    assert parsed.get("error") == "query_failed", (
        f"{label}: wrapper should find upsc and report query_failed, got {parsed}"
    )
    assert "invocation_failed" not in parsed.get("error", ""), parsed
    err = machine.succeed(f"cat {stderr_path}")
    assert err == "", f"{label}: expected empty stderr under --json, got {err!r}"


with subtest("module wrapper finds upsc with empty PATH"):
    assert_wrapper_finds_upsc("module-wrapper", shlex.quote(module_braid))

with subtest("top-level package wrapper finds upsc with empty PATH"):
    top_level_braid = machine.succeed("cat /etc/braid/top-level-braid-path").strip()
    assert_wrapper_finds_upsc("top-level-wrapper", shlex.quote(top_level_braid))
