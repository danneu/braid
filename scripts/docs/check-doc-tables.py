#!/usr/bin/env python3
"""Validate guide/command table parity across SUMMARY.md, docs/index.md, and README.md.

Source-of-truth rules:
  - Each guide file's H1 owns its title.
  - Each command file's H1 owns its bare command name, and its frontmatter owns
    whether the label is experimental.
  - Command link labels are "🧪 " plus the bare command name when
    `experimental: true`, else the bare command name.
  - SUMMARY.md owns the canonical ordering; index.md and README.md must follow it.

Membership is checked transitively: the existing `check-docs` bash recipe verifies
SUMMARY.md against the files on disk, and this script verifies README.md and
docs/index.md against SUMMARY.md -- so if both pass, all three lists agree with disk.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
LINK_RE = re.compile(r"\[([^\]]+)\]\(([^)]+\.md)\)")
FRONTMATTER_RE = re.compile(r"\A---\n(.*?)\n---\n", re.DOTALL)
EXPERIMENTAL_EMOJI = "🧪"


def read_h1(path: Path) -> str:
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("# "):
            return line[2:].strip()
    raise RuntimeError(f"{path} has no H1 heading")


def read_experimental(path: Path) -> str | None:
    match = FRONTMATTER_RE.match(path.read_text(encoding="utf-8"))
    if match is None:
        return None
    for line in match.group(1).splitlines():
        key, sep, value = line.partition(":")
        if sep and key.strip() == "experimental":
            return value.strip()
    return None


def expected_label(errors: list[str], kind: str, path: Path, h1: str) -> str:
    if kind == "commands":
        name = h1.removeprefix("braid ")
        experimental = read_experimental(path)
        if experimental not in {"true", "false"}:
            got = "missing" if experimental is None else experimental
            rel = path.relative_to(ROOT / "docs")
            errors.append(
                f"{rel}: experimental frontmatter must be exactly true or false (got {got!r})"
            )
        return f"{EXPERIMENTAL_EMOJI} {name}" if experimental == "true" else name
    return h1


def section_body(text: str, heading_line: str) -> str:
    """Return text below an exact heading line, until the next markdown heading."""
    body: list[str] = []
    inside = False
    for line in text.splitlines():
        if line == heading_line:
            inside = True
            continue
        if inside:
            if re.match(r"^#{1,6} ", line):
                break
            body.append(line)
    return "\n".join(body)


def section_links(text: str, heading_line: str, prefix: str) -> list[tuple[str, str]]:
    body = section_body(text, heading_line)
    return [(label, href) for label, href in LINK_RE.findall(body) if href.startswith(prefix)]


def normalize(href: str) -> str:
    return href.removeprefix("docs/")


def compare(errors: list[str], context: str, canonical: list[tuple[str, str]], actual: list[tuple[str, str]]) -> None:
    canonical_files = [f for _, f in canonical]
    actual_files = [f for _, f in actual]
    if canonical_files != actual_files:
        errors.append(f"{context}: ordering or membership differs from SUMMARY.md")
        errors.append(f"  SUMMARY.md: {canonical_files}")
        errors.append(f"  {context}: {actual_files}")
        return
    for (elabel, efile), (alabel, _) in zip(canonical, actual):
        if elabel != alabel:
            errors.append(f"{context}: {efile} label {alabel!r} != canonical {elabel!r}")


def collect_canonical(errors: list[str], summary_text: str, heading: str, prefix: str, kind: str) -> list[tuple[str, str]]:
    canonical: list[tuple[str, str]] = []
    for label, href in section_links(summary_text, heading, prefix):
        file_key = normalize(href)
        path = ROOT / "docs" / file_key
        h1 = read_h1(path)
        want = expected_label(errors, kind, path, h1)
        if label != want:
            errors.append(f"SUMMARY.md: {file_key} label {label!r} != canonical {want!r}")
        canonical.append((want, file_key))
    return canonical


def main() -> int:
    summary = (ROOT / "docs" / "SUMMARY.md").read_text(encoding="utf-8")
    index = (ROOT / "docs" / "index.md").read_text(encoding="utf-8")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")

    errors: list[str] = []

    canonical_guides = collect_canonical(errors, summary, "# Guides", "guides/", "guides")
    canonical_commands = collect_canonical(errors, summary, "# Commands", "commands/", "commands")

    index_guides = [(label, normalize(href)) for label, href in section_links(index, "## Guides", "guides/")]
    index_commands = [(label, normalize(href)) for label, href in section_links(index, "## Commands", "commands/")]
    compare(errors, "docs/index.md guides", canonical_guides, index_guides)
    compare(errors, "docs/index.md commands", canonical_commands, index_commands)

    readme_guides = [(label, normalize(href)) for label, href in section_links(readme, "### Guides", "docs/guides/")]
    readme_commands = [(label, normalize(href)) for label, href in section_links(readme, "### Commands", "docs/commands/")]
    compare(errors, "README.md guides", canonical_guides, readme_guides)
    compare(errors, "README.md commands", canonical_commands, readme_commands)

    if errors:
        for e in errors:
            print(e)
        return 1
    print("doc-table parity ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
