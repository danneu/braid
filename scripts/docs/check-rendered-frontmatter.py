#!/usr/bin/env python3
"""Fail if rendered mdBook HTML exposes source YAML frontmatter."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
HTML_ROOT = ROOT / "docs/book/html"
LEAK_PATTERN = re.compile(r"(<p>|<br ?/?>|^)(intent|status|experimental):", re.MULTILINE)


def main() -> int:
    if not HTML_ROOT.is_dir():
        print(f"missing rendered docs directory: {HTML_ROOT.relative_to(ROOT)}", file=sys.stderr)
        return 1

    html_files = sorted(HTML_ROOT.rglob("*.html"))
    if not html_files:
        print(f"rendered docs directory has no HTML files: {HTML_ROOT.relative_to(ROOT)}", file=sys.stderr)
        return 1

    matches: list[str] = []
    for path in html_files:
        text = path.read_text(encoding="utf-8")
        for match in LEAK_PATTERN.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            snippet = match.group(0).strip()
            matches.append(f"{path.relative_to(ROOT)}:{line}:{snippet}")

    if matches:
        print("rendered frontmatter leaked into HTML:", file=sys.stderr)
        for match in matches:
            print(f"  {match}", file=sys.stderr)
        return 1

    print("rendered frontmatter check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
