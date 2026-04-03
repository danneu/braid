#!/usr/bin/env python3
"""Fetch reference source and docs at the versions pinned in flake.lock."""

import json
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent.parent


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


def locked_nixpkgs_flakeref() -> str:
    """Read flake.lock and return a flake reference to the exact pinned nixpkgs rev."""
    lock = json.loads((ROOT / "flake.lock").read_text())
    rev = lock["nodes"]["nixpkgs"]["locked"]["rev"]
    return f"github:NixOS/nixpkgs/{rev}"


def nix_version(nixpkgs: str, attr: str) -> str:
    result = subprocess.run(
        ["nix", "eval", "--raw", f"{nixpkgs}#{attr}.version"],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def fetch_source_repos(staging: Path, nixpkgs: str) -> None:
    """Shallow-clone upstream repos into a staging directory."""
    for dep in DEPS:
        version = nix_version(nixpkgs, dep.nix_attr)
        tag = dep.tag(version)
        target = staging / dep.nix_attr
        print(f"{dep.nix_attr}: version {version} → tag {tag}")

        subprocess.run(
            [
                "git",
                "-c",
                "advice.detachedHead=false",
                "clone",
                "--depth",
                "1",
                "--quiet",
                "--branch",
                tag,
                dep.repo,
                str(target),
            ],
            check=True,
        )


def inline_btrfs_docs(staging: Path) -> None:
    """Inline ch-*.rst fragments in btrfs-progs/Documentation/."""
    docs = staging / "btrfs-progs" / "Documentation"

    for f in docs.glob("*.rst"):
        text = f.read_text()

        def inline(m: re.Match[str]) -> str:
            inc = docs / m.group(1)
            return inc.read_text() if inc.exists() else m.group(0)

        text = re.sub(r"^\.\. include:: (ch-[^\s]+)$", inline, text, flags=re.MULTILINE)
        f.write_text(text)

    fragments = list(docs.glob("ch-*.rst"))
    for f in fragments:
        f.unlink()

    count = len(list(docs.glob("**/*.rst")))
    print(f"btrfs-docs: {count} RST files, {len(fragments)} ch-* fragments inlined")


def main() -> None:
    nixpkgs = locked_nixpkgs_flakeref()
    dest = ROOT / "reference"

    with tempfile.TemporaryDirectory(dir=ROOT, prefix=".reference-staging-") as tmp:
        staging = Path(tmp)
        fetch_source_repos(staging, nixpkgs)
        inline_btrfs_docs(staging)

        # All clones succeeded — swap via backup so dest is never absent.
        backup = ROOT / ".reference-backup"
        if backup.exists():
            shutil.rmtree(backup)
        if dest.exists():
            dest.rename(backup)
        try:
            staging.rename(dest)
        except BaseException:
            if not dest.exists() and backup.exists():
                backup.rename(dest)
            raise
        if backup.exists():
            shutil.rmtree(backup)

    print("\nDone.")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nInterrupted.", file=sys.stderr)
        raise SystemExit(130)
