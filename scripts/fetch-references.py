#!/usr/bin/env python3
"""Fetch reference source and docs at pinned versions.

Includes: btrfs-progs, systemd, autosuspend, cryptsetup, util-linux, smartmontools, hddfancontrol, nut, coreutils, linux kernel, nix crate.
Most resources are pinned by flake.lock; Rust crate sources are pinned by Cargo.lock.
The linux kernel tarball is ~140MB and may take a few minutes to download and extract.

Usage:
  python3 scripts/fetch-references.py            # Fetch all resources
  python3 scripts/fetch-references.py linux      # Fetch only linux kernel
  python3 scripts/fetch-references.py nix-crate  # Fetch only nix Rust crate
  python3 scripts/fetch-references.py --list     # List available resources
"""

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Literal

ROOT = Path(__file__).resolve().parent.parent


@dataclass
class Dep:
    nix_attr: str
    fetch_type: Literal["git", "tarball", "cargo"]
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
    Dep("nut", "git", "https://github.com/networkupstools/nut.git", lambda v: f"v{v}"),
    Dep("coreutils", "git", "https://github.com/coreutils/coreutils.git", lambda v: f"v{v}"),
    Dep("linux", "tarball", "", lambda v: v),
    Dep("nix-crate", "cargo", "", lambda v: v),
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


def read_nix_crate_pin() -> tuple[str, str]:
    """Resolve the nix crate version through braid-cli's direct Cargo.lock edge."""
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    packages = lock.get("package", [])

    braid_cli_packages = [pkg for pkg in packages if pkg.get("name") == "braid-cli"]
    if len(braid_cli_packages) != 1:
        raise RuntimeError("Cargo.lock must contain exactly one braid-cli package")

    nix_dep: re.Match[str] | None = None
    for dep in braid_cli_packages[0].get("dependencies", []):
        match = re.fullmatch(r"nix(?: (?P<version>[^\s(]+))?(?: \((?P<source>[^)]+)\))?", dep)
        if match is not None:
            if nix_dep is not None:
                raise RuntimeError("braid-cli has multiple Cargo.lock dependencies named nix")
            nix_dep = match

    if nix_dep is None:
        raise RuntimeError("braid-cli does not depend on nix in Cargo.lock")

    version = nix_dep.group("version")
    source = nix_dep.group("source")
    candidates = [pkg for pkg in packages if pkg.get("name") == "nix"]

    if version is not None:
        candidates = [pkg for pkg in candidates if pkg.get("version") == version]
    if source is not None:
        candidates = [pkg for pkg in candidates if pkg.get("source") == source]

    if not candidates:
        qualifier = "nix" if version is None else f"nix {version}"
        raise RuntimeError(f"Cargo.lock has no package entry for {qualifier}")
    if len(candidates) > 1:
        raise RuntimeError(
            "Cargo.lock has multiple nix packages; braid-cli dependency must specify a version"
        )

    package = candidates[0]
    package_version = package.get("version")
    checksum = package.get("checksum")
    if not isinstance(package_version, str):
        raise RuntimeError("nix package in Cargo.lock is missing version")
    if not isinstance(checksum, str):
        raise RuntimeError("nix package in Cargo.lock is missing checksum")

    return package_version, checksum


def fetch_cargo_crate(target: Path) -> None:
    """Download and extract the nix crate pinned by Cargo.lock."""
    version, checksum = read_nix_crate_pin()
    url = f"https://static.crates.io/crates/nix/nix-{version}.crate"
    print(f"nix-crate: version {version}")
    print(f"  → downloading {url}")

    with tempfile.NamedTemporaryFile(suffix=".crate", delete=False) as tmp_crate:
        tmp_crate_path = Path(tmp_crate.name)

    try:
        urllib.request.urlretrieve(url, tmp_crate_path)
        actual_checksum = hashlib.sha256(tmp_crate_path.read_bytes()).hexdigest()
        if actual_checksum != checksum:
            raise RuntimeError(
                "sha256 mismatch for nix "
                f"{version}: expected {checksum}, got {actual_checksum}"
            )

        print(f"  → extracting to {target}")

        with tarfile.open(tmp_crate_path, "r:gz") as tar:
            with tempfile.TemporaryDirectory() as extract_tmp:
                tar.extractall(extract_tmp, filter="data")
                extract_path = Path(extract_tmp) / f"nix-{version}"
                if not extract_path.is_dir():
                    raise RuntimeError(f"nix {version} crate did not contain nix-{version}/")
                shutil.move(str(extract_path), str(target))
    finally:
        if tmp_crate_path.exists():
            tmp_crate_path.unlink()


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
        target = staging / dep.nix_attr

        if dep.fetch_type == "git":
            version = nix_version(nixpkgs, dep.nix_attr)
            print(f"{dep.nix_attr}: version {version}")
            fetch_git_repo(dep, version, target)
        elif dep.fetch_type == "tarball":
            version = nix_version(nixpkgs, dep.nix_attr)
            print(f"{dep.nix_attr}: version {version}")
            fetch_tarball(nixpkgs, target)
        elif dep.fetch_type == "cargo":
            fetch_cargo_crate(target)
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
        description="Fetch reference source and docs at pinned versions"
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

    needs_nixpkgs = any(dep.fetch_type != "cargo" for dep in deps)
    nixpkgs = locked_nixpkgs_flakeref() if needs_nixpkgs else ""
    dest = ROOT / "reference"
    subset = args.resource is not None

    with tempfile.TemporaryDirectory(dir=ROOT, prefix=".reference-staging-") as tmp:
        staging = Path(tmp)
        fetch_source_repos(staging, nixpkgs, deps)
        inline_btrfs_docs(staging, deps)

        if not subset:
            # All fetches succeeded -- swap via backup so dest is never absent.
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
        else:
            # Subset fetch -- per-dep swap so unfetched deps in reference/
            # are preserved.
            dest.mkdir(exist_ok=True)
            for dep in deps:
                staged_dir = staging / dep.nix_attr
                target_dir = dest / dep.nix_attr
                backup_dir = dest / f".{dep.nix_attr}.backup"

                if backup_dir.exists():
                    shutil.rmtree(backup_dir)
                if target_dir.exists():
                    target_dir.rename(backup_dir)
                try:
                    staged_dir.rename(target_dir)
                except BaseException:
                    if not target_dir.exists() and backup_dir.exists():
                        backup_dir.rename(target_dir)
                    raise
                if backup_dir.exists():
                    shutil.rmtree(backup_dir)

    print("\nDone.")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nInterrupted.", file=sys.stderr)
        raise SystemExit(130)
