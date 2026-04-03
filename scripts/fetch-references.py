#!/usr/bin/env python3
"""Shallow-clone vendored reference repos at the versions pinned in nixpkgs."""

import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / "reference"


@dataclass
class Dep:
    nix_attr: str
    repo: str
    tag: Callable[[str], str]


DEPS = [
    Dep("btrfs-progs", "https://github.com/kdave/btrfs-progs.git", lambda v: f"v{v}"),
    Dep("systemd", "https://github.com/systemd/systemd.git", lambda v: f"v{v}"),
    Dep("autosuspend", "https://github.com/languitar/autosuspend.git", lambda v: f"v{v}"),
    Dep("cryptsetup", "https://gitlab.com/cryptsetup/cryptsetup.git", lambda v: f"v{v}"),
    Dep("util-linux", "https://github.com/util-linux/util-linux.git", lambda v: f"v{v}"),
    Dep(
        "smartmontools",
        "https://github.com/smartmontools/smartmontools.git",
        lambda v: f"RELEASE_{v.replace('.', '_')}",
    ),
]


def nix_version(attr: str) -> str:
    result = subprocess.run(
        ["nix", "eval", "--raw", f"nixpkgs#{attr}.version"],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def main() -> None:
    for dep in DEPS:
        version = nix_version(dep.nix_attr)
        tag = dep.tag(version)
        target = DEST / dep.nix_attr
        print(f"{dep.nix_attr}: version {version} → tag {tag}")

        if target.exists():
            shutil.rmtree(target)

        subprocess.run(
            [
                "git",
                "-c", "advice.detachedHead=false",
                "clone", "--depth", "1", "--quiet",
                "--branch", tag, dep.repo, str(target),
            ],
            check=True,
        )

    print(f"\nDone. Reference repos updated in {DEST}/")


if __name__ == "__main__":
    main()
