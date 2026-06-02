#!/usr/bin/env python3
"""Flag markdown links that escape the docs/ subtree (rendered-broken in mdBook).

The previous `check-docs` grep matched any `](../../` textually, which false-flags
legitimately-nested pages: a depth-2 page like docs/internals/btrfs/balance-soft.md
needs `../../` to reach the docs root, and that link resolves to a valid in-book
page. mdbook-linkcheck2 already verifies in-book targets exist; the only escape
this check must still catch is a relative link whose normalized target climbs
*above* docs/, which renders broken on the deployed site.

A link escapes iff `normpath(join(dir_of_file, target))` starts with `..`. This is
depth-aware by construction, so it accepts `../../` from a nested page while still
rejecting a chain that actually leaves docs/.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
DOCS = ROOT / "docs"
# Inline markdown link targets: `](target ...)`. Capture up to the first space
# (an optional "title") or closing paren. Matches the old grep's inline-link
# scope -- reference-style `[label]: target` definitions are intentionally
# ignored, as they were before.
LINK_RE = re.compile(r"\]\(\s*([^)\s]+)")


def is_relative_link(target: str) -> bool:
    """Only relative links can escape; skip URLs, anchors, and root-absolute paths."""
    if target.startswith(("#", "/")):
        return False
    if "://" in target or target.startswith(("mailto:", "tel:")):
        return False
    return True


def escapes(rel_dir: str, target: str) -> bool:
    """True when the link, resolved against its file's dir, climbs above docs/."""
    # Judge by path alone -- drop `#anchor` / `?query` so `../../x.md#frag` is fine.
    path = target.split("#", 1)[0].split("?", 1)[0]
    if not path:
        return False
    resolved = os.path.normpath(os.path.join(rel_dir, path))
    return resolved == ".." or resolved.startswith(".." + os.sep)


def main() -> int:
    offenders: list[str] = []
    for path in sorted(DOCS.rglob("*.md")):
        # Skip the mdBook build output (docs/book/), as the old grep did.
        if path.relative_to(DOCS).parts[0] == "book":
            continue
        rel_dir = os.path.dirname(path.relative_to(DOCS).as_posix())
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            for target in LINK_RE.findall(line):
                if is_relative_link(target) and escapes(rel_dir, target):
                    rel = path.relative_to(ROOT).as_posix()
                    offenders.append(f"{rel}:{lineno}: {target}")

    if offenders:
        print("markdown links escape docs/ subtree (broken in rendered mdBook):")
        for o in offenders:
            print(f"  {o}")
        return 1
    print("doc-link escape check ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
