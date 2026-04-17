#!/usr/bin/env python3
"""Fetch reference source and docs at the versions pinned in flake.lock.

Includes: btrfs-progs, systemd, autosuspend, cryptsetup, util-linux, smartmontools, hddfancontrol, linux kernel.
The linux kernel tarball is ~140MB and may take a few minutes to download and extract.

Usage:
  python3 scripts/fetch-references.py            # Fetch all resources
  python3 scripts/fetch-references.py linux      # Fetch only linux kernel
  python3 scripts/fetch-references.py --list     # List available resources
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Literal

ROOT = Path(__file__).resolve().parent.parent


@dataclass
class Dep:
    nix_attr: str
    fetch_type: Literal["git", "tarball"]
    repo: str
    tag: Callable[[str], str]


DEPS = [
    Dep("btrfs-progs", "git", "https://github.com/kdave/btrfs-progs.git", lambda v: f"v{v}"),
    Dep("systemd", "git", "https://github.com/systemd/systemd.git", lambda v: f"v{v}"),
    Dep("autosuspend", "git", "https://github.com/languitar/autosuspend.git", lambda v: f"v{v}"),
    Dep("cryptsetup", "git", "https://gitlab.com/cryptsetup/cryptsetup.git", lambda v: f"v{v}"),
    Dep("util-linux", "git", "https://github.com/util-linux/util-linux.git", lambda v: f"v{v}"),
    Dep(
        "smartmontools",
        "git",
        "https://github.com/smartmontools/smartmontools.git",
        lambda v: f"RELEASE_{v.replace('.', '_')}",
    ),
    Dep("hddfancontrol", "git", "https://github.com/desbma/hddfancontrol.git", lambda v: v),
    Dep("linux", "tarball", "", lambda v: v),
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


def nix_eval(nixpkgs: str, expr: str) -> str:
    """Evaluate a nix expression and return the result as raw text."""
    result = subprocess.run(
        ["nix", "eval", "--raw", f"{nixpkgs}#{expr}"],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def fetch_git_repo(dep: Dep, version: str, target: Path) -> None:
    """Shallow-clone a git repository."""
    tag = dep.tag(version)
    print(f"  → cloning tag {tag}")
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


def fetch_tarball(nixpkgs: str, target: Path) -> None:
    """Download and extract a tarball source from nixpkgs."""
    # Get the src URL from nixpkgs
    url = nix_eval(nixpkgs, "linux.src.url")

    # Expand mirror:// URLs (nixpkgs-specific)
    if url.startswith("mirror://kernel/"):
        path = url.replace("mirror://kernel/", "")
        url = f"https://cdn.kernel.org/pub/{path}"

    print(f"  → downloading {url}")

    with tempfile.NamedTemporaryFile(suffix=".tar.xz", delete=False) as tmp_tar:
        tmp_tar_path = Path(tmp_tar.name)

    try:
        urllib.request.urlretrieve(url, tmp_tar_path)
        print(f"  → extracting to {target}")

        with tarfile.open(tmp_tar_path, "r:xz") as tar:
            # Extract all members, stripping the top-level directory
            members = tar.getmembers()
            if members:
                # Find the common prefix (top-level directory)
                prefix = members[0].name.split('/')[0]

                # Extract to staging, then move contents
                with tempfile.TemporaryDirectory() as extract_tmp:
                    tar.extractall(extract_tmp, filter="data")
                    extract_path = Path(extract_tmp) / prefix

                    # Move the extracted directory to target
                    shutil.move(str(extract_path), str(target))
    finally:
        if tmp_tar_path.exists():
            tmp_tar_path.unlink()


def get_dep_by_name(name: str) -> Dep | None:
    """Look up a dependency by nix_attr name."""
    for dep in DEPS:
        if dep.nix_attr == name:
            return dep
    return None


def filter_deps(resource_name: str | None) -> list[Dep]:
    """Filter DEPS list. If resource_name is None, return all. Otherwise return just that resource."""
    if resource_name is None:
        return DEPS

    dep = get_dep_by_name(resource_name)
    if dep is None:
        available = ", ".join(d.nix_attr for d in DEPS)
        raise ValueError(f"Unknown resource '{resource_name}'. Available: {available}")
    return [dep]


def fetch_source_repos(staging: Path, nixpkgs: str, deps: list[Dep]) -> None:
    """Fetch upstream source repos into a staging directory."""
    for dep in deps:
        version = nix_version(nixpkgs, dep.nix_attr)
        target = staging / dep.nix_attr
        print(f"{dep.nix_attr}: version {version}")

        if dep.fetch_type == "git":
            fetch_git_repo(dep, version, target)
        elif dep.fetch_type == "tarball":
            fetch_tarball(nixpkgs, target)
        else:
            raise ValueError(f"Unknown fetch type: {dep.fetch_type}")


def inline_btrfs_docs(staging: Path, deps: list[Dep]) -> None:
    """Inline ch-*.rst fragments in btrfs-progs/Documentation/ if btrfs-progs is being fetched."""
    # Only process if btrfs-progs is in the deps list
    if not any(dep.nix_attr == "btrfs-progs" for dep in deps):
        return

    docs = staging / "btrfs-progs" / "Documentation"
    if not docs.exists():
        return

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
    parser = argparse.ArgumentParser(
        description="Fetch reference source and docs at versions pinned in flake.lock"
    )
    parser.add_argument(
        "resource",
        nargs="?",
        help="specific resource to fetch (optional; if omitted, fetch all)",
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="list available resources and exit",
    )

    args = parser.parse_args()

    # Handle --list flag
    if args.list:
        print("Available resources:")
        for dep in DEPS:
            print(f"  {dep.nix_attr}")
        return

    # Validate and filter resources
    try:
        deps = filter_deps(args.resource)
    except ValueError as e:
        print(f"Error: {e}", file=sys.stderr)
        raise SystemExit(1)

    nixpkgs = locked_nixpkgs_flakeref()
    dest = ROOT / "reference"

    with tempfile.TemporaryDirectory(dir=ROOT, prefix=".reference-staging-") as tmp:
        staging = Path(tmp)
        fetch_source_repos(staging, nixpkgs, deps)
        inline_btrfs_docs(staging, deps)

        # All fetches succeeded — swap via backup so dest is never absent.
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
