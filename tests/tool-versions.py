import json

start_all()
machine.wait_for_unit("multi-user.target")

# Load expected versions from Nix evaluation
expected = json.loads(machine.succeed("cat /etc/braid/expected-versions.json"))

# Provenance: assert tools resolve to /nix/store/ paths (not ambient PATH leaks)
for tool in ["btrfs", "cryptsetup", "findmnt", "lsblk", "mountpoint"]:
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

# Rust binary wrapper provenance
with subtest("braid provenance"):
    path = machine.succeed("readlink -f $(command -v braid)").strip()
    assert path.startswith("/nix/store/"), f"braid not from nix store: {path}"
